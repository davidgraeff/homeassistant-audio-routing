<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import { createDnd } from '../lib/dnd.svelte';
  import { routing } from '../lib/routing';
  import type { AnnouncementGroup, MusicGroup, OutputInfo, RoutingNode } from '../lib/types';

  const NONE = '(none)';
  const MIXED = '(mixed)';

  let loading = $state(true);
  let outputs = $state<OutputInfo[]>([]);
  let music = $state<MusicGroup[]>([]);
  let announcement = $state<AnnouncementGroup[]>([]);

  // Live routing matrix (shared WebSocket store) — so link edits made in the
  // flow graph or via the API reflect on this page immediately and honestly,
  // rather than from a stale one-shot fetch.
  let matrix = $derived($routing.matrix);

  // Which mixed-routing groups have their per-member breakdown expanded.
  let expandedMixed = $state<Set<string>>(new Set());

  // New music group scratch row.
  let mgName = $state('');
  let mgMembers = $state<Set<string>>(new Set());
  let mgBusy = $state(false);

  // New announcement group scratch row.
  let agName = $state('');
  let agTargets = $state<Set<string>>(new Set());
  let agPriority = $state<number>(0);
  let agDuck = $state<number>(0.25);
  let agBusy = $state(false);

  // In-place rename: which group's title is being edited.
  let editing = $state<{ section: 'music' | 'ann'; id: string; name: string } | null>(null);

  // Outputs claimed by some music group or the music scratch row (exclusive).
  let musicClaimed = $derived(new Set([...music.flatMap((g) => g.members), ...mgMembers]));
  // The music "Available" pool: everything not yet in a music group or the scratch row.
  let musicPool = $derived(outputs.filter((o) => !musicClaimed.has(o.node_name)));

  function displayName(nodeName: string): string {
    return outputs.find((o) => o.node_name === nodeName)?.name ?? nodeName;
  }

  async function refresh() {
    loading = true;
    try {
      const [o, m, a] = await Promise.all([api.outputs(), api.musicGroups(), api.announcementGroups()]);
      outputs = o;
      music = m;
      announcement = a;
    } catch {
      // a toast already surfaced the error
    }
    loading = false;
  }
  onMount(refresh);

  // ── drag-and-drop ────────────────────────────────────────────────────────
  // Payload identifies the dragged device and where it came from, so a drop can
  // remove it from its origin. Zone ids: `m:pool` | `m:g:<id>` | `m:new` for
  // music, `a:pool` | `a:g:<id>` | `a:new` for announcements.
  type DragPayload = { section: 'music' | 'ann'; node: string; from: string };

  const dnd = createDnd<DragPayload>((payload, zone) => {
    if (!zone || zone === payload.from) return;
    if (!zone.startsWith(payload.section === 'music' ? 'm:' : 'a:')) return; // no cross-section drops
    if (payload.section === 'music') void onMusicDrop(payload.node, payload.from, zone);
    else void onAnnDrop(payload.node, payload.from, zone);
  });

  const withNode = (s: Set<string>, k: string) => new Set(s).add(k);
  const withoutNode = (s: Set<string>, k: string) => {
    const n = new Set(s);
    n.delete(k);
    return n;
  };
  const groupIdOf = (zone: string, prefix: string) => (zone.startsWith(prefix) ? zone.slice(prefix.length) : null);

  // Music membership is exclusive: a drop moves the device out of its origin
  // and into the target (pool means "no group").
  async function onMusicDrop(node: string, from: string, to: string) {
    const jobs: Promise<boolean>[] = [];
    if (from === 'm:new') mgMembers = withoutNode(mgMembers, node);
    else {
      const fromId = groupIdOf(from, 'm:g:');
      if (fromId) {
        const g = music.find((x) => x.id === fromId);
        if (g) jobs.push(run(() => api.updateMusicGroup(fromId, { members: g.members.filter((m) => m !== node) })));
      }
    }
    if (to === 'm:new') mgMembers = withNode(mgMembers, node);
    else {
      const toId = groupIdOf(to, 'm:g:');
      if (toId) {
        const g = music.find((x) => x.id === toId);
        if (g && !g.members.includes(node)) jobs.push(run(() => api.updateMusicGroup(toId, { members: [...g.members, node] })));
      }
    }
    if (jobs.length) {
      await Promise.all(jobs);
      await refresh();
    }
  }

  // Announcement membership overlaps freely. The pool is always full, so a drag
  // from `a:pool` is a copy (origin untouched); a drag from a group is a move.
  async function onAnnDrop(node: string, from: string, to: string) {
    const jobs: Promise<boolean>[] = [];
    if (from === 'a:new') agTargets = withoutNode(agTargets, node);
    else {
      const fromId = groupIdOf(from, 'a:g:');
      if (fromId) {
        const g = announcement.find((x) => x.id === fromId);
        if (g) jobs.push(run(() => api.updateAnnouncementGroup(fromId, { targets: g.targets.filter((t) => t !== node) })));
      }
    }
    if (to === 'a:new') agTargets = withNode(agTargets, node);
    else {
      const toId = groupIdOf(to, 'a:g:');
      if (toId) {
        const g = announcement.find((x) => x.id === toId);
        if (g && !g.targets.includes(node)) jobs.push(run(() => api.updateAnnouncementGroup(toId, { targets: [...g.targets, node] })));
      }
    }
    if (jobs.length) {
      await Promise.all(jobs);
      await refresh();
    }
  }

  // ── rename ───────────────────────────────────────────────────────────────
  function startRename(section: 'music' | 'ann', id: string, name: string) {
    editing = { section, id, name };
  }
  function focus(node: HTMLInputElement) {
    node.focus();
    node.select();
  }
  async function commitRename() {
    if (!editing) return;
    const { section, id, name } = editing;
    const trimmed = name.trim();
    const current = section === 'music' ? music.find((g) => g.id === id)?.name : announcement.find((g) => g.id === id)?.name;
    editing = null;
    if (!trimmed || trimmed === current) return;
    const ok =
      section === 'music'
        ? await run(() => api.updateMusicGroup(id, { name: trimmed }), `Renamed to "${trimmed}"`)
        : await run(() => api.updateAnnouncementGroup(id, { name: trimmed }), `Renamed to "${trimmed}"`);
    if (ok) await refresh();
  }
  function onRenameKey(e: KeyboardEvent) {
    if (e.key === 'Enter') (e.currentTarget as HTMLInputElement).blur();
    else if (e.key === 'Escape') editing = null;
  }

  // ── source routing (music groups) ─────────────────────────────────────────
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
  async function setMusicGroupSource(g: MusicGroup, displayName: string) {
    if (displayName === MIXED) return; // synthetic current value; not selectable
    if (displayName === NONE) {
      if (await run(() => api.unrouteMusicGroup(g.id), `"${g.name}" un-routed`)) await refresh();
      return;
    }
    const src = matrix.sources.find((s) => s.display_name === displayName);
    if (!src) return;
    if (await run(() => api.routeMusicGroup(g.id, src.node_name), `"${g.name}" now playing ${displayName}`)) await refresh();
  }

  // ── announcement priority / duck (inline edit of existing groups) ──────────
  async function setAnnPriority(g: AnnouncementGroup, priority: number) {
    if (priority === g.priority) return;
    if (await run(() => api.updateAnnouncementGroup(g.id, { priority: Number(priority) }))) await refresh();
  }
  async function setAnnDuck(g: AnnouncementGroup, duck: number) {
    if (duck === g.duck) return;
    if (await run(() => api.updateAnnouncementGroup(g.id, { duck: Number(duck) }))) await refresh();
  }

  // ── create / delete / test ─────────────────────────────────────────────────
  async function createMusicGroup() {
    if (!mgName.trim() || mgMembers.size === 0) return;
    mgBusy = true;
    if (await run(() => api.createMusicGroup(mgName.trim(), [...mgMembers]), `Music group "${mgName}" created`)) {
      mgName = '';
      mgMembers = new Set();
      await refresh();
    }
    mgBusy = false;
  }
  async function deleteMusicGroup(g: MusicGroup) {
    if (await run(() => api.deleteMusicGroup(g.id), `Deleted "${g.name}"`)) await refresh();
  }
  async function createAnnouncementGroup() {
    if (!agName.trim() || agTargets.size === 0) return;
    agBusy = true;
    if (
      await run(
        () => api.createAnnouncementGroup(agName.trim(), [...agTargets], Number(agPriority), Number(agDuck)),
        `Announcement group "${agName}" created`,
      )
    ) {
      agName = '';
      agTargets = new Set();
      agPriority = 0;
      agDuck = 0.25;
      await refresh();
    }
    agBusy = false;
  }
  async function deleteAnnouncementGroup(g: AnnouncementGroup) {
    if (await run(() => api.deleteAnnouncementGroup(g.id), `Deleted "${g.name}"`)) await refresh();
  }
  async function testAnnounce(g: AnnouncementGroup) {
    await run(() => api.announceToGroup(g.id), `Test announcement sent to "${g.name}"`);
  }
</script>

<svelte:window onpointermove={dnd.move} onpointerup={dnd.end} onpointercancel={dnd.end} />

{#snippet chip(node: string, from: string, section: 'music' | 'ann')}
  <span
    class="chip"
    role="button"
    tabindex="0"
    title={displayName(node)}
    onpointerdown={(e) => dnd.begin(e, { section, node, from }, displayName(node))}
  >
    <svg class="grip" viewBox="0 0 16 16" aria-hidden="true"><circle cx="5" cy="4" r="1.3" /><circle cx="11" cy="4" r="1.3" /><circle cx="5" cy="8" r="1.3" /><circle cx="11" cy="8" r="1.3" /><circle cx="5" cy="12" r="1.3" /><circle cx="11" cy="12" r="1.3" /></svg>
    {displayName(node)}
  </span>
{/snippet}

{#snippet titleHead(section: 'music' | 'ann', id: string, name: string)}
  {#if editing && editing.section === section && editing.id === id}
    <input class="rename" bind:value={editing.name} use:focus onblur={commitRename} onkeydown={onRenameKey} />
  {:else}
    <button class="title" onclick={() => startRename(section, id, name)} title="Rename group">
      <strong>{name}</strong>
      <svg class="pencil" viewBox="0 0 16 16" aria-hidden="true"><path d="M11.5 1.5l3 3L5 14l-3.5.5L2 11z" /></svg>
    </button>
  {/if}
{/snippet}

{#if loading}
  <div class="card"><p class="empty">Loading…</p></div>
{:else}
  <div class="card">
    <h2>Music groups</h2>
    <p class="card-sub">
      A set of speakers that play the same audio in sync. Each speaker belongs to <strong>one</strong> music group — drag
      it from <em>Available</em> into a group, between groups, or back to release it.
    </p>

    <div class="pool" data-dropzone="m:pool" class:hover={dnd.active && dnd.payload?.section === 'music' && dnd.hover === 'm:pool'}>
      <span class="pool-label">Available</span>
      {#each musicPool as o (o.node_name)}
        {@render chip(o.node_name, 'm:pool', 'music')}
      {/each}
      {#if musicPool.length === 0}<span class="hint">All speakers are in a group</span>{/if}
    </div>

    {#each music as g (g.id)}
      {@const r = mgRouting(g)}
      <div class="grouprow" data-dropzone={`m:g:${g.id}`} class:hover={dnd.active && dnd.payload?.section === 'music' && dnd.hover === `m:g:${g.id}`}>
        <div class="grouptop">
          {@render titleHead('music', g.id, g.name)}
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
          <label class="srcpick">
            <span class="muted">Source</span>
            <select value={selectValue(r)} onchange={(e) => setMusicGroupSource(g, e.currentTarget.value)}>
              {#if r.state === 'mixed'}<option value={MIXED} disabled>Mixed</option>{/if}
              <option value={NONE}>{NONE}</option>
              {#each matrix.sources as s (s.node_name)}
                <option value={s.display_name}>{s.display_name}</option>
              {/each}
            </select>
          </label>
          <button class="danger" onclick={() => deleteMusicGroup(g)}>Delete</button>
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
          </div>
        {/if}
        <div class="chips">
          {#each g.members as n (n)}
            {@render chip(n, `m:g:${g.id}`, 'music')}
          {/each}
          {#if g.members.length === 0}<span class="hint">Drop speakers here</span>{/if}
        </div>
      </div>
    {/each}

    <div class="grouprow scratch" data-dropzone="m:new" class:hover={dnd.active && dnd.payload?.section === 'music' && dnd.hover === 'm:new'}>
      <div class="grouptop">
        <input class="newname" type="text" placeholder="New music group name" bind:value={mgName} />
        <button class="primary" onclick={createMusicGroup} disabled={mgBusy || !mgName.trim() || mgMembers.size === 0}>Create</button>
      </div>
      <div class="chips">
        {#each [...mgMembers] as n (n)}
          {@render chip(n, 'm:new', 'music')}
        {/each}
        {#if mgMembers.size === 0}<span class="hint">Drop speakers here to start a new group</span>{/if}
      </div>
    </div>
  </div>

  <div class="card">
    <h2>Announcement groups</h2>
    <p class="card-sub">
      A reusable set of speakers for announcements, with a priority and how far music ducks while it plays. A speaker may
      appear in any number of announcement groups — drag from <em>Available</em> to add a copy, or drag a chip out to remove it.
    </p>

    <div class="pool" data-dropzone="a:pool" class:hover={dnd.active && dnd.payload?.section === 'ann' && dnd.hover === 'a:pool'}>
      <span class="pool-label">Available</span>
      {#each outputs as o (o.node_name)}
        {@render chip(o.node_name, 'a:pool', 'ann')}
      {/each}
      {#if outputs.length === 0}<span class="hint">No speakers configured</span>{/if}
    </div>

    {#each announcement as g (g.id)}
      <div class="grouprow" data-dropzone={`a:g:${g.id}`} class:hover={dnd.active && dnd.payload?.section === 'ann' && dnd.hover === `a:g:${g.id}`}>
        <div class="grouptop">
          {@render titleHead('ann', g.id, g.name)}
          <div class="field">
            <span class="muted">Priority</span>
            <input type="number" step="1" value={g.priority} onchange={(e) => setAnnPriority(g, Number(e.currentTarget.value))} />
          </div>
          <div class="field">
            <span class="muted">Duck {g.duck.toFixed(2)}</span>
            <input type="range" min="0" max="1" step="0.05" value={g.duck} onchange={(e) => setAnnDuck(g, Number(e.currentTarget.value))} />
          </div>
          <button onclick={() => testAnnounce(g)}>Test</button>
          <button class="danger" onclick={() => deleteAnnouncementGroup(g)}>Delete</button>
        </div>
        <div class="chips">
          {#each g.targets as n (n)}
            {@render chip(n, `a:g:${g.id}`, 'ann')}
          {/each}
          {#if g.targets.length === 0}<span class="hint">Drop speakers here</span>{/if}
        </div>
      </div>
    {/each}

    <div class="grouprow scratch" data-dropzone="a:new" class:hover={dnd.active && dnd.payload?.section === 'ann' && dnd.hover === 'a:new'}>
      <div class="grouptop">
        <input class="newname" type="text" placeholder="New announcement group name" bind:value={agName} />
        <div class="field">
          <span class="muted">Priority</span>
          <input type="number" step="1" bind:value={agPriority} />
        </div>
        <div class="field">
          <span class="muted">Duck {Number(agDuck).toFixed(2)}</span>
          <input type="range" min="0" max="1" step="0.05" bind:value={agDuck} />
        </div>
        <button class="primary" onclick={createAnnouncementGroup} disabled={agBusy || !agName.trim() || agTargets.size === 0}>Create</button>
      </div>
      <div class="chips">
        {#each [...agTargets] as n (n)}
          {@render chip(n, 'a:new', 'ann')}
        {/each}
        {#if agTargets.size === 0}<span class="hint">Drop speakers here to start a new group</span>{/if}
      </div>
    </div>
  </div>
{/if}

{#if dnd.active}
  <div class="ghost" style={`left:${dnd.x}px; top:${dnd.y}px`}>{dnd.label}</div>
{/if}

<style>
  .pool,
  .grouprow {
    border: 1px solid rgba(127, 127, 127, 0.25);
    border-radius: 8px;
    padding: 10px 12px;
    margin-bottom: 10px;
    transition:
      border-color 0.12s,
      background 0.12s;
  }
  .pool {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    background: var(--secondary-background-color, rgba(127, 127, 127, 0.08));
  }
  .pool-label {
    font-size: 0.8rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    opacity: 0.7;
    margin-right: 4px;
  }
  .grouprow.scratch {
    border-style: dashed;
  }
  /* Highlight the drop target the pointer is over. */
  .pool.hover,
  .grouprow.hover {
    border-color: var(--primary-color, #03a9f4);
    background: color-mix(in srgb, var(--primary-color, #03a9f4) 12%, transparent);
  }

  .grouptop {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .title {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
    background: none;
    border: none;
    padding: 2px 4px;
    margin: 0;
    cursor: text;
    color: inherit;
    font: inherit;
    text-align: left;
    border-radius: 6px;
  }
  .title:hover {
    background: rgba(127, 127, 127, 0.12);
  }
  .title strong {
    font-size: 1rem;
  }
  .pencil {
    width: 13px;
    height: 13px;
    fill: currentColor;
    opacity: 0.45;
    flex: 0 0 auto;
  }
  .title:hover .pencil {
    opacity: 0.8;
  }
  .rename {
    flex: 1;
    min-width: 0;
    font: inherit;
    font-weight: 700;
    padding: 3px 6px;
    border-radius: 6px;
    border: 1px solid var(--primary-color, #03a9f4);
  }
  .newname {
    flex: 1;
    min-width: 140px;
    padding: 5px 8px;
    border-radius: 6px;
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-top: 8px;
    min-height: 30px;
    align-items: center;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 4px 9px 4px 5px;
    border-radius: 999px;
    background: var(--card-background-color, #fff);
    border: 1px solid rgba(127, 127, 127, 0.35);
    font-size: 0.85rem;
    cursor: grab;
    touch-action: none; /* let us own the drag gesture on touch */
    user-select: none;
    -webkit-user-select: none;
  }
  .chip:active {
    cursor: grabbing;
  }
  .grip {
    width: 13px;
    height: 13px;
    fill: currentColor;
    opacity: 0.4;
    flex: 0 0 auto;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .field input[type='number'] {
    width: 70px;
    padding: 3px 6px;
    border-radius: 6px;
  }
  .field input[type='range'] {
    width: 130px;
  }
  .muted {
    opacity: 0.65;
    font-size: 0.78rem;
  }
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
  .srcpick {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .srcpick select {
    padding: 4px 6px;
    border-radius: 6px;
  }
  .hint {
    opacity: 0.55;
    font-size: 0.82rem;
    font-style: italic;
  }

  /* Ghost chip that follows the pointer during a drag. */
  .ghost {
    position: fixed;
    z-index: 1000;
    transform: translate(8px, 8px);
    pointer-events: none;
    padding: 4px 10px;
    border-radius: 999px;
    background: var(--primary-color, #03a9f4);
    color: var(--text-primary-color, #fff);
    font-size: 0.85rem;
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.3);
    opacity: 0.95;
  }
</style>
