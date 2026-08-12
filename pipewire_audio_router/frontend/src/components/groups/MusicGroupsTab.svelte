<script lang="ts">
  // Music groups: sets of speakers that play the same audio in sync, plus what
  // each group is currently playing. Membership is exclusive (a speaker is in at
  // most one music group), so the pool and the group cards are one exclusive
  // drag-and-drop surface.
  //
  // Page shape mirrors Announcements: one explanation card, then the speaker pool
  // beside the dock that turns a dropped speaker into a group, then the group
  // cards two to a row (newest first). Long-form detail lives in the docs dialog.
  //
  // The routing graph below is the same routing, one altitude lower: it shows
  // and edits the individual source→speaker links a group-level route expands
  // into. Editing a single member there is what produces the "⚠ Mixed" state a
  // group card can report.
  import { onMount } from 'svelte';
  import { flip } from 'svelte/animate';
  import { api } from '../../lib/api';
  import { run } from '../../lib/toast';
  import { createDnd } from '../../lib/dnd.svelte';
  import { routing } from '../../lib/routing';
  import DeviceChip from './DeviceChip.svelte';
  import GroupTitle from './GroupTitle.svelte';
  import FlowGraph from '../routing/FlowGraph.svelte';
  import MusicGroupDocs from './MusicGroupDocs.svelte';
  import type { MusicGroup, OutputInfo, RoutingNode } from '../../lib/types';

  const NONE = '(none)';
  const MIXED = '(mixed)';

  let loading = $state(true);
  let outputs = $state<OutputInfo[]>([]);
  let music = $state<MusicGroup[]>([]);
  let docsOpen = $state(false);

  // Live routing matrix (shared WebSocket store) — so link edits made in the
  // flow graph or via the API reflect on this page immediately and honestly,
  // rather than from a stale one-shot fetch.
  let matrix = $derived($routing.matrix);

  // Which mixed-routing groups have their per-member breakdown expanded.
  let expandedMixed = $state<Set<string>>(new Set());

  // Newest first: a group appears directly below the dock that created it.
  let shown = $derived([...music].reverse());

  // The speaker currently being turned into a group (drives the dock animation),
  // and the group that just arrived (drives its drop-in animation). The dock is
  // held in its sunk state a moment past the request so the hand-over is visible
  // even when the daemon answers instantly.
  let creating = $state<string | null>(null);
  let arriving = $state<string | null>(null);
  let arriveTimer: ReturnType<typeof setTimeout> | undefined;
  let launchTimer: ReturnType<typeof setTimeout> | undefined;

  const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const flipMs = reduceMotion ? 0 : 220;

  // Outputs claimed by some music group (exclusive).
  let claimed = $derived(new Set(music.flatMap((g) => g.members)));
  // The "Available" pool: everything not yet in a group.
  let pool = $derived(outputs.filter((o) => !claimed.has(o.node_name)));

  function displayName(nodeName: string): string {
    return outputs.find((o) => o.node_name === nodeName)?.name ?? nodeName;
  }

  /** First unused "Group N" — a dock-created group is named, not asked about. */
  function defaultName(): string {
    for (let n = 1; ; n++) {
      const name = `Group ${n}`;
      if (!music.some((g) => g.name === name)) return name;
    }
  }

  // `quiet` keeps the page mounted while it reloads, so a drop doesn't blink the
  // whole list away (and the arrival animations survive).
  async function refresh(quiet = false) {
    if (!quiet) loading = true;
    try {
      const [o, m] = await Promise.all([api.outputs(), api.musicGroups()]);
      outputs = o;
      music = m;
    } catch {
      // a toast already surfaced the error
    }
    loading = false;
  }
  onMount(() => {
    void refresh();
    return () => {
      clearTimeout(arriveTimer);
      clearTimeout(launchTimer);
    };
  });

  // ── drag-and-drop ────────────────────────────────────────────────────────
  // Payload identifies the dragged device and where it came from, so a drop can
  // remove it from its origin. Zone ids: `pool` | `g:<id>` | `new`.
  type DragPayload = { node: string; from: string };

  const dnd = createDnd<DragPayload>((payload, zone) => {
    if (!zone || zone === payload.from) return;
    void onDrop(payload.node, payload.from, zone);
  });

  const groupIdOf = (zone: string) => (zone.startsWith('g:') ? zone.slice(2) : null);

  // Membership is exclusive: a drop moves the device out of its origin and into
  // the target (the pool means "no group").
  async function onDrop(node: string, from: string, to: string) {
    if (to === 'new') {
      await createFrom(node, from);
      return;
    }
    const jobs: Promise<boolean>[] = [];
    const fromId = groupIdOf(from);
    if (fromId) {
      const g = music.find((x) => x.id === fromId);
      if (g) jobs.push(run(() => api.updateMusicGroup(fromId, { members: g.members.filter((m) => m !== node) })));
    }
    const toId = groupIdOf(to);
    if (toId) {
      const g = music.find((x) => x.id === toId);
      if (g && !g.members.includes(node)) jobs.push(run(() => api.updateMusicGroup(toId, { members: [...g.members, node] })));
    }
    if (jobs.length) {
      await Promise.all(jobs);
      await refresh(true);
    }
  }

  /** The dock: one dropped speaker becomes a whole group. The release from its
   *  old group has to land first — membership is exclusive, so creating while
   *  it's still elsewhere is refused by the daemon. */
  async function createFrom(node: string, from: string) {
    if (creating) return;
    creating = node;
    const fromId = groupIdOf(from);
    if (fromId) {
      const g = music.find((x) => x.id === fromId);
      if (g && !(await run(() => api.updateMusicGroup(fromId, { members: g.members.filter((m) => m !== node) })))) {
        creating = null;
        await refresh(true);
        return;
      }
    }
    const name = defaultName();
    let created: MusicGroup | undefined;
    const ok = await run(async () => {
      const res = await api.createMusicGroup(name, [node]);
      created = res.group;
      return res;
    }, `Music group "${name}" created`);
    if (!ok) {
      creating = null;
      await refresh(true); // the release above may have landed — show the truth
      return;
    }
    await refresh(true);
    // Flag the fresh card so it animates in, and let the dock ease back once it
    // has handed over; both clear themselves when the animations are done.
    arriving = created?.id ?? null;
    clearTimeout(arriveTimer);
    arriveTimer = setTimeout(() => (arriving = null), 400);
    clearTimeout(launchTimer);
    launchTimer = setTimeout(() => (creating = null), 220);
  }

  async function rename(g: MusicGroup, name: string) {
    if (await run(() => api.updateMusicGroup(g.id, { name }), `Renamed to "${name}"`)) await refresh(true);
  }

  // ── source routing ────────────────────────────────────────────────────────
  // A group's members are each routed independently in the matrix, so the
  // group-level source is one of: none | a single source shared by every member
  // (uniform) | mixed (some members unrouted, or members on different sources).
  // Manual edits in the flow graph or via the API can produce the mixed state.
  type GroupRouting =
    | { state: 'none' }
    | { state: 'uniform'; source: RoutingNode }
    | { state: 'mixed'; perMember: { output: string; sources: RoutingNode[] }[] };

  function srcNode(name: string): RoutingNode {
    return (
      matrix.sources.find((s) => s.node_name === name) ??
      ({ node_name: name, display_name: name, present: false, configured: true, node_id: null, peak: 0 } as RoutingNode)
    );
  }

  function mgRouting(g: MusicGroup): GroupRouting {
    const perMember = g.members.map((m) => ({
      output: m,
      sources: matrix.links.filter((l) => l.output === m).map((l) => srcNode(l.source)),
    }));
    const routed = perMember.filter((p) => p.sources.length > 0);
    if (routed.length === 0) return { state: 'none' };
    const uniqueSources = new Set(perMember.flatMap((p) => p.sources.map((s) => s.node_name)));
    const allSingle = perMember.every((p) => p.sources.length === 1);
    if (routed.length === g.members.length && allSingle && uniqueSources.size === 1) {
      return { state: 'uniform', source: perMember[0].sources[0] };
    }
    return { state: 'mixed', perMember };
  }

  const selectValue = (r: GroupRouting) => (r.state === 'uniform' ? r.source.display_name : r.state === 'mixed' ? MIXED : NONE);

  function toggleBreakdown(id: string) {
    const next = new Set(expandedMixed);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    expandedMixed = next;
  }

  // Picking a real source reconciles the group: the daemon links that source to
  // every member and removes any other source feeding them (see route handler).
  async function setSource(g: MusicGroup, displayName: string) {
    if (displayName === MIXED) return; // synthetic current value; not selectable
    if (displayName === NONE) {
      if (await run(() => api.unrouteMusicGroup(g.id), `"${g.name}" un-routed`)) await refresh(true);
      return;
    }
    const src = matrix.sources.find((s) => s.display_name === displayName);
    if (!src) return;
    if (await run(() => api.routeMusicGroup(g.id, src.node_name), `"${g.name}" now playing ${displayName}`)) await refresh(true);
  }

  async function remove(g: MusicGroup) {
    if (await run(() => api.deleteMusicGroup(g.id), `Deleted "${g.name}"`)) await refresh(true);
  }
</script>

<svelte:window onpointermove={dnd.move} onpointerup={dnd.end} onpointercancel={dnd.end} />

<div class="card">
  <div class="card-head">
    <h2>Music groups</h2>
    <button class="ghost" type="button" title="Exclusive membership, sources, and the mixed state" onclick={() => (docsOpen = true)}>
      Explain music groups
    </button>
  </div>
  <p class="card-sub" style="margin-bottom:0">
    Speakers that play the same audio in sync, and one Home Assistant <code>media_player</code> each. A speaker belongs
    to <strong>one</strong> music group; <strong>Source</strong> picks what the whole group plays.
  </p>
</div>

{#if loading}
  <div class="card"><p class="empty">Loading…</p></div>
{:else}
  <div class="groupgrid head">
    <div class="card dropzone" data-dropzone="pool" class:hover={dnd.active && dnd.hover === 'pool'}>
      <div class="card-head">
        <strong>Available</strong>
        <span class="hint">Not in any group — drag one in</span>
      </div>
      <div class="pool">
        {#each pool as o (o.node_name)}
          <DeviceChip label={displayName(o.node_name)} onDragStart={(e) => dnd.begin(e, { node: o.node_name, from: 'pool' }, displayName(o.node_name))} />
        {/each}
        {#if pool.length === 0}<span class="drop-hint">All speakers are in a group</span>{/if}
      </div>
    </div>

    <!-- The dock: no name field. One drop makes a real group, which the card below
         then names, routes and fills. -->
    <div class="card dropzone dock" data-dropzone="new" class:hover={dnd.active && dnd.hover === 'new'} class:launching={!!creating}>
      <div class="dockrow">
        <strong>New group</strong>
        {#if creating}
          <span class="hint">Creating a group for {displayName(creating)}…</span>
        {:else}
          <span class="hint">Drop a speaker here</span>
        {/if}
      </div>
    </div>
  </div>

  {#if shown.length}
    <div class="groupgrid">
      {#each shown as g (g.id)}
        {@const r = mgRouting(g)}
        <div
          class="card dropzone groupcard"
          class:hover={dnd.active && dnd.hover === `g:${g.id}`}
          class:arriving={arriving === g.id}
          data-dropzone={`g:${g.id}`}
          animate:flip={{ duration: flipMs }}
        >
          <div class="grouptop">
            <GroupTitle name={g.name} onRename={(name) => rename(g, name)} />
            {#if r.state === 'mixed'}
              <button
                class="mixed"
                onclick={() => toggleBreakdown(g.id)}
                aria-expanded={expandedMixed.has(g.id)}
                title="Members are routed inconsistently — click for details"
              >
                ⚠ Mixed
                <svg class="caret" class:open={expandedMixed.has(g.id)} viewBox="0 0 16 16" aria-hidden="true"><path d="M4 6l4 4 4-4" /></svg>
              </button>
            {/if}
          </div>
          {#if r.state === 'mixed' && expandedMixed.has(g.id)}
            <div class="breakdown">
              {#each r.perMember as pm (pm.output)}
                <div class="brow">
                  <span class="bname" title={displayName(pm.output)}>{displayName(pm.output)}</span>
                  <span class="barrow">←</span>
                  {#if pm.sources.length}
                    <span class="bsrc">{pm.sources.map((s) => s.display_name).join(' + ')}</span>
                  {:else}
                    <span class="bsrc muted">unrouted</span>
                  {/if}
                </div>
              {/each}
              <p class="bfix">Pick a source above to put every member back on one stream.</p>
            </div>
          {/if}
          <div class="chips">
            {#each g.members as n (n)}
              <DeviceChip label={displayName(n)} onDragStart={(e) => dnd.begin(e, { node: n, from: `g:${g.id}` }, displayName(n))} />
            {/each}
            {#if g.members.length === 0}<span class="drop-hint">Drop speakers here</span>{/if}
          </div>
          <div class="groupactions">
            <label class="ctlrow">
              <span class="ctllabel">Source</span>
              <select value={selectValue(r)} onchange={(e) => setSource(g, e.currentTarget.value)}>
                {#if r.state === 'mixed'}<option value={MIXED} disabled>Mixed</option>{/if}
                <option value={NONE}>{NONE}</option>
                {#each matrix.sources as s (s.node_name)}
                  <option value={s.display_name}>{s.display_name}</option>
                {/each}
              </select>
            </label>
            <button class="danger" onclick={() => remove(g)}>Delete</button>
          </div>
        </div>
      {/each}
    </div>
  {/if}

  <!-- Same routing, drawn as wires: the graph's right column is these groups, so
       a wire onto one is the same call as its Source dropdown above. -->
  <FlowGraph groups={shown} />
{/if}

{#if docsOpen}
  <MusicGroupDocs onClose={() => (docsOpen = false)} />
{/if}

{#if dnd.active}
  <div class="drag-ghost" style={`left:${dnd.x}px; top:${dnd.y}px`}>{dnd.label}</div>
{/if}

<style>
  /* The "⚠ Mixed" pill and its per-member breakdown — this page only: it exists
     because a group's members can be routed individually in the graph below. */
  .mixed {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 3px 8px;
    border-radius: 999px;
    border: 1px solid var(--warning-color, #f0a202);
    background: color-mix(in srgb, var(--warning-color, #f0a202) 14%, transparent);
    color: var(--warning-color, #f0a202);
    font-size: 0.78rem;
    font-weight: 600;
    cursor: pointer;
  }
  .caret {
    width: 12px;
    height: 12px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    transition: transform 0.12s;
  }
  .caret.open {
    transform: rotate(180deg);
  }
  .breakdown {
    margin-top: 8px;
    padding: 8px 10px;
    border-radius: 8px;
    border: 1px dashed color-mix(in srgb, var(--warning-color, #f0a202) 45%, transparent);
    background: color-mix(in srgb, var(--warning-color, #f0a202) 7%, transparent);
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .brow {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.85rem;
  }
  .bname {
    min-width: 0;
    flex: 0 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 500;
  }
  .barrow {
    opacity: 0.6;
    flex: 0 0 auto;
  }
  .bsrc {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .bfix {
    margin: 4px 0 0;
    font-size: 0.78rem;
    opacity: 0.75;
  }
  /* The dock is one line high — a target, not a form. */
  .dockrow {
    display: flex;
    align-items: baseline;
    gap: 10px;
    flex-wrap: wrap;
  }
</style>
