<script lang="ts">
  // Announcement groups: reusable sets of speakers a clip (TTS, doorbell, alarm)
  // is played to, each with a priority and how far music ducks while it plays.
  // Unlike music groups these overlap freely — a speaker may be in any number of
  // them — so the pool always lists every speaker and dragging from it copies.
  //
  // The page is a stack of first-class cards: one explanation, the speaker pool,
  // the dock that turns a dropped speaker into a new group, then one card per
  // group (newest first, right under the dock it came out of). The long-form
  // detail lives in the docs dialog, not on the page.
  import { onMount } from 'svelte';
  import { flip } from 'svelte/animate';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import { createDnd } from '../lib/dnd.svelte';
  import DeviceChip from './DeviceChip.svelte';
  import GroupTitle from './GroupTitle.svelte';
  import AnnouncementDocs from './AnnouncementDocs.svelte';
  import type { AnnouncementGroup, OutputInfo } from '../lib/types';

  let loading = $state(true);
  let outputs = $state<OutputInfo[]>([]);
  let groups = $state<AnnouncementGroup[]>([]);
  let docsOpen = $state(false);

  // Newest first: a group appears directly below the dock that created it.
  let shown = $derived([...groups].reverse());

  // Priority a dock-created group starts with (its duck level is left to the
  // daemon's configured default); both are editable on the group's own card.
  const NEW_PRIORITY = 0;

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

  function displayName(nodeName: string): string {
    return outputs.find((o) => o.node_name === nodeName)?.name ?? nodeName;
  }

  /** First unused "Announcement N" — a dock-created group is named, not asked about. */
  function defaultName(): string {
    for (let n = 1; ; n++) {
      const name = `Announcement ${n}`;
      if (!groups.some((g) => g.name === name)) return name;
    }
  }

  // `quiet` keeps the page mounted while it reloads, so a drop doesn't blink the
  // whole list away (and the arrival animations survive).
  async function refresh(quiet = false) {
    if (!quiet) loading = true;
    try {
      const [o, a] = await Promise.all([api.outputs(), api.announcementGroups()]);
      outputs = o;
      groups = a;
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
  // Zone ids: `pool` | `g:<id>` | `new`. Membership overlaps, so the pool is
  // always full: a drag from `pool` is a copy (origin untouched); a drag from a
  // group is a move (dropping it back on the pool removes it).
  type DragPayload = { node: string; from: string };

  const dnd = createDnd<DragPayload>((payload, zone) => {
    if (!zone || zone === payload.from) return;
    void onDrop(payload.node, payload.from, zone);
  });

  const groupIdOf = (zone: string) => (zone.startsWith('g:') ? zone.slice(2) : null);

  async function onDrop(node: string, from: string, to: string) {
    if (to === 'new') {
      await createFrom(node, from);
      return;
    }
    const jobs: Promise<boolean>[] = [];
    const fromId = groupIdOf(from);
    if (fromId) {
      const g = groups.find((x) => x.id === fromId);
      if (g) jobs.push(run(() => api.updateAnnouncementGroup(fromId, { targets: g.targets.filter((t) => t !== node) })));
    }
    const toId = groupIdOf(to);
    if (toId) {
      const g = groups.find((x) => x.id === toId);
      if (g && !g.targets.includes(node)) jobs.push(run(() => api.updateAnnouncementGroup(toId, { targets: [...g.targets, node] })));
    }
    if (jobs.length) {
      await Promise.all(jobs);
      await refresh(true);
    }
  }

  /** The dock: one dropped speaker becomes a whole group with a default name. */
  async function createFrom(node: string, from: string) {
    if (creating) return;
    creating = node;
    const fromId = groupIdOf(from);
    if (fromId) {
      const g = groups.find((x) => x.id === fromId);
      if (g) await run(() => api.updateAnnouncementGroup(fromId, { targets: g.targets.filter((t) => t !== node) }));
    }
    const name = defaultName();
    let created: AnnouncementGroup | undefined;
    const ok = await run(async () => {
      const res = await api.createAnnouncementGroup(name, [node], NEW_PRIORITY);
      created = res.group;
      return res;
    }, `Announcement group "${name}" created`);
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

  // ── inline edits ──────────────────────────────────────────────────────────
  async function rename(g: AnnouncementGroup, name: string) {
    if (await run(() => api.updateAnnouncementGroup(g.id, { name }), `Renamed to "${name}"`)) await refresh(true);
  }
  async function setPriority(g: AnnouncementGroup, priority: number) {
    if (priority === g.priority) return;
    if (await run(() => api.updateAnnouncementGroup(g.id, { priority: Number(priority) }))) await refresh(true);
  }
  async function setDuck(g: AnnouncementGroup, duck: number) {
    if (duck === g.duck) return;
    if (await run(() => api.updateAnnouncementGroup(g.id, { duck: Number(duck) }))) await refresh(true);
  }

  // ── delete / test ─────────────────────────────────────────────────────────
  async function remove(g: AnnouncementGroup) {
    if (await run(() => api.deleteAnnouncementGroup(g.id), `Deleted "${g.name}"`)) await refresh(true);
  }
  async function testAnnounce(g: AnnouncementGroup) {
    await run(() => api.announceToGroup(g.id), `Test announcement sent to "${g.name}"`);
  }
</script>

<svelte:window onpointermove={dnd.move} onpointerup={dnd.end} onpointercancel={dnd.end} />

<div class="card">
  <div class="card-head">
    <h2>Announcements</h2>
    <button class="ghost" type="button" title="Ducking, priority, overlapping groups, and how to play to one" onclick={() => (docsOpen = true)}>
      Explain announcement groups
    </button>
  </div>
  <p class="card-sub" style="margin-bottom:0">
    A reusable set of speakers a clip is played to — one Home Assistant <code>media_player</code> each, so
    <code>tts.speak</code> against it reaches all of them. The clip is overlaid on whatever they're already playing:
    <strong>Duck</strong> is how far that music drops, <strong>Priority</strong> who wins when two clips collide.
  </p>
</div>

{#if loading}
  <div class="card"><p class="empty">Loading…</p></div>
{:else}
  <div class="card dropzone" data-dropzone="pool" class:hover={dnd.active && dnd.hover === 'pool'}>
    <div class="card-head">
      <h2>Available</h2>
      <span class="hint">Every speaker — drag one into as many groups as you like</span>
    </div>
    <div class="pool">
      {#each outputs as o (o.node_name)}
        <DeviceChip label={displayName(o.node_name)} onDragStart={(e) => dnd.begin(e, { node: o.node_name, from: 'pool' }, displayName(o.node_name))} />
      {/each}
      {#if outputs.length === 0}<span class="drop-hint">No speakers configured</span>{/if}
    </div>
  </div>

  <!-- The dock: no name, no priority, no duck. One drop makes a real group with
       defaults, which the card below then edits. -->
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

  {#each shown as g (g.id)}
    <div
      class="card dropzone"
      class:hover={dnd.active && dnd.hover === `g:${g.id}`}
      class:arriving={arriving === g.id}
      data-dropzone={`g:${g.id}`}
      animate:flip={{ duration: flipMs }}
    >
      <div class="grouptop">
        <GroupTitle name={g.name} onRename={(name) => rename(g, name)} />
        <div class="field">
          <span class="muted">Priority</span>
          <input type="number" step="1" value={g.priority} onchange={(e) => setPriority(g, Number(e.currentTarget.value))} />
        </div>
        <div class="field">
          <span class="muted">Duck {g.duck.toFixed(2)}</span>
          <input type="range" min="0" max="1" step="0.05" value={g.duck} onchange={(e) => setDuck(g, Number(e.currentTarget.value))} />
        </div>
        <button onclick={() => testAnnounce(g)} title="Play the built-in test clip to this group now">Test</button>
        <button class="danger" onclick={() => remove(g)}>Delete</button>
      </div>
      <div class="chips">
        {#each g.targets as n (n)}
          <DeviceChip label={displayName(n)} onDragStart={(e) => dnd.begin(e, { node: n, from: `g:${g.id}` }, displayName(n))} />
        {/each}
        {#if g.targets.length === 0}<span class="drop-hint">Drop speakers here</span>{/if}
      </div>
    </div>
  {/each}
{/if}

{#if docsOpen}
  <AnnouncementDocs onClose={() => (docsOpen = false)} />
{/if}

{#if dnd.active}
  <div class="drag-ghost" style={`left:${dnd.x}px; top:${dnd.y}px`}>{dnd.label}</div>
{/if}

<style>
  /* Priority / duck controls inside a group card: label above a narrow input. */
  .field {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin-bottom: 0;
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
  /* The dock is one line high — a target, not a form. */
  .dockrow {
    display: flex;
    align-items: baseline;
    gap: 10px;
    flex-wrap: wrap;
  }
</style>
