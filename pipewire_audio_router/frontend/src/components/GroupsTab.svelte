<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { AnnouncementGroup, MusicGroup, OutputInfo, RoutingMatrix } from '../lib/types';

  const NONE = '(none)';

  let loading = $state(true);
  let outputs = $state<OutputInfo[]>([]);
  let music = $state<MusicGroup[]>([]);
  let announcement = $state<AnnouncementGroup[]>([]);
  let routing = $state<RoutingMatrix>({ sources: [], outputs: [], links: [] });

  // New music group form.
  let mgName = $state('');
  let mgMembers = $state<Set<string>>(new Set());
  let mgBusy = $state(false);

  // New announcement group form.
  let agName = $state('');
  let agTargets = $state<Set<string>>(new Set());
  let agPriority = $state<number>(0);
  let agDuck = $state<number>(0.25);
  let agBusy = $state(false);

  // Outputs already claimed by some music group (exclusive — can't join another).
  let claimed = $derived(new Set(music.flatMap((g) => g.members)));

  function displayName(nodeName: string): string {
    return outputs.find((o) => o.node_name === nodeName)?.name ?? nodeName;
  }

  async function refresh() {
    loading = true;
    try {
      const [o, m, a, r] = await Promise.all([api.outputs(), api.musicGroups(), api.announcementGroups(), api.routing()]);
      outputs = o;
      music = m;
      announcement = a;
      routing = r;
    } catch {
      // a toast already surfaced the error
    }
    loading = false;
  }
  onMount(refresh);

  // The source currently routed to a music group's members (display name), or
  // NONE. Reads the live routing links; reflects the group as a whole.
  function mgSource(g: MusicGroup): string {
    const memberSet = new Set(g.members);
    const srcNodes = new Set(routing.links.filter((l) => memberSet.has(l.output)).map((l) => l.source));
    const first = routing.sources.find((s) => srcNodes.has(s.node_name));
    return first ? first.display_name : NONE;
  }

  async function setMusicGroupSource(g: MusicGroup, displayName: string) {
    if (displayName === NONE) {
      if (await run(() => api.unrouteMusicGroup(g.id), `"${g.name}" un-routed`)) await refresh();
      return;
    }
    const src = routing.sources.find((s) => s.display_name === displayName);
    if (!src) return;
    if (await run(() => api.routeMusicGroup(g.id, src.node_name), `"${g.name}" now playing ${displayName}`)) await refresh();
  }

  function toggle(set: Set<string>, key: string): Set<string> {
    const next = new Set(set);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    return next;
  }

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

{#if loading}
  <div class="card"><p class="empty">Loading…</p></div>
{:else}
  <div class="card">
    <h2>Music groups</h2>
    <p class="card-sub">
      A set of speakers that play the same audio in sync. Each speaker can belong to <strong>one</strong> music group.
      Route audio to the group as a whole (routing integration is rolling out).
    </p>

    {#if music.length === 0}
      <p class="empty">No music groups yet.</p>
    {:else}
      <ul class="glist">
        {#each music as g (g.id)}
          <li>
            <div class="ginfo">
              <strong>{g.name}</strong>
              <span class="members">{g.members.map(displayName).join(', ') || '(no members)'}</span>
            </div>
            <label class="srcpick">
              <span class="muted">Source</span>
              <select value={mgSource(g)} onchange={(e) => setMusicGroupSource(g, e.currentTarget.value)}>
                <option value={NONE}>{NONE}</option>
                {#each routing.sources as s (s.node_name)}
                  <option value={s.display_name}>{s.display_name}</option>
                {/each}
              </select>
            </label>
            <button class="danger" onclick={() => deleteMusicGroup(g)}>Delete</button>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="newform">
      <input type="text" placeholder="New music group name" bind:value={mgName} />
      <div class="picker">
        {#each outputs as o (o.node_name)}
          {@const inThisForm = mgMembers.has(o.node_name)}
          {@const takenElsewhere = claimed.has(o.node_name)}
          <label class="check" class:disabled={takenElsewhere}>
            <input
              type="checkbox"
              checked={inThisForm}
              disabled={takenElsewhere}
              onchange={() => (mgMembers = toggle(mgMembers, o.node_name))}
            />
            {o.name}
            {#if takenElsewhere}<span class="muted">(in another group)</span>{/if}
          </label>
        {/each}
      </div>
      <button class="primary" onclick={createMusicGroup} disabled={mgBusy || !mgName.trim() || mgMembers.size === 0}>
        Create music group
      </button>
    </div>
  </div>

  <div class="card">
    <h2>Announcement groups</h2>
    <p class="card-sub">
      A reusable set of speakers for announcements, with a priority and how far music ducks while it plays. Speakers may
      appear in any number of announcement groups, regardless of their music group.
    </p>

    {#if announcement.length === 0}
      <p class="empty">No announcement groups yet.</p>
    {:else}
      <ul class="glist">
        {#each announcement as g (g.id)}
          <li>
            <div class="ginfo">
              <strong>{g.name}</strong>
              <span class="members">{g.targets.map(displayName).join(', ') || '(no targets)'}</span>
              <span class="muted">priority {g.priority} · duck {g.duck.toFixed(2)}</span>
            </div>
            <button onclick={() => testAnnounce(g)}>Test</button>
            <button class="danger" onclick={() => deleteAnnouncementGroup(g)}>Delete</button>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="newform">
      <input type="text" placeholder="New announcement group name" bind:value={agName} />
      <div class="picker">
        {#each outputs as o (o.node_name)}
          <label class="check">
            <input type="checkbox" checked={agTargets.has(o.node_name)} onchange={() => (agTargets = toggle(agTargets, o.node_name))} />
            {o.name}
          </label>
        {/each}
      </div>
      <div class="row">
        <div class="field" style="flex:0 0 120px">
          <label for="ag-priority">Priority</label>
          <input id="ag-priority" type="number" step="1" bind:value={agPriority} />
        </div>
        <div class="field" style="flex:0 0 160px">
          <label for="ag-duck">Duck: {Number(agDuck).toFixed(2)}</label>
          <input id="ag-duck" type="range" min="0" max="1" step="0.05" bind:value={agDuck} />
        </div>
      </div>
      <button class="primary" onclick={createAnnouncementGroup} disabled={agBusy || !agName.trim() || agTargets.size === 0}>
        Create announcement group
      </button>
    </div>
  </div>
{/if}

<style>
  .glist {
    list-style: none;
    margin: 0 0 12px;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .glist li {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 10px;
    background: var(--secondary-background-color, rgba(127, 127, 127, 0.08));
    border-radius: 8px;
  }
  .ginfo {
    display: flex;
    flex-direction: column;
    gap: 2px;
    flex: 1;
    min-width: 0;
  }
  .members {
    font-size: 0.85rem;
    opacity: 0.85;
  }
  .muted {
    opacity: 0.65;
    font-size: 0.8rem;
  }
  .newform {
    display: flex;
    flex-direction: column;
    gap: 10px;
    border-top: 1px solid rgba(127, 127, 127, 0.25);
    padding-top: 12px;
  }
  .picker {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 16px;
  }
  .check.disabled {
    opacity: 0.5;
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
  .row {
    display: flex;
    gap: 16px;
    align-items: flex-end;
  }
</style>
