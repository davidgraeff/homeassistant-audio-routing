<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { routing } from '../lib/routing';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { AlignGroup, AlignMember, AlignState } from '../lib/types';

  let groups = $state<AlignGroup[]>([]);
  let session = $state<AlignState | null>(null);
  let loading = $state(true);
  let busy = $state(false);
  // Current per-member offset in ms (sendspin static delay / RAOP latency).
  let offsets = $state<Record<string, number>>({});

  const active = $derived(session?.active ?? false);

  // Friendly names from the routing matrix (falls back to the raw node name).
  function label(nodeName: string): string {
    const all = [...$routing.matrix.outputs, ...$routing.matrix.sources];
    return all.find((n) => n.node_name === nodeName)?.display_name ?? nodeName;
  }

  function sliderMax(m: AlignMember): number {
    return m.kind === 'sendspin' ? 2000 : 5000;
  }

  async function seedOffsets() {
    const next: Record<string, number> = {};
    try {
      const [delays, outs] = await Promise.all([
        api.sendspinDelays().catch(() => ({}) as Record<string, number>),
        api.outputs().catch(() => []),
      ]);
      for (const m of session?.members ?? []) {
        if (m.kind === 'sendspin') next[m.node_name] = delays[m.node_name] ?? 0;
        else next[m.node_name] = outs.find((o) => o.node_name === m.node_name)?.latency_ms ?? 0;
      }
    } catch {
      /* leave defaults */
    }
    offsets = next;
  }

  async function refresh() {
    loading = true;
    try {
      const [st, gs] = await Promise.all([api.alignStatus(), api.alignGroups()]);
      session = st;
      groups = gs;
      if (st.active) await seedOffsets();
    } catch {
      session = null;
      groups = [];
    }
    loading = false;
  }
  onMount(refresh);

  // Safety: if the tab is torn down while a session is active, stop it so the
  // daemon doesn't keep speakers muted with the click looping (the server-side
  // timeout is the ultimate backstop, but this is immediate).
  onDestroy(() => {
    if (session?.active) void api.alignStop().catch(() => {});
  });

  async function start(group: AlignGroup) {
    busy = true;
    try {
      session = await api.alignStart(group.sources);
      await seedOffsets();
    } catch (e) {
      await run(() => Promise.reject(e));
    }
    busy = false;
  }

  async function stop() {
    busy = true;
    if (await run(() => api.alignStop(), 'Alignment finished — volumes restored')) {
      session = { active: false, sources: [], reference: null, target: null, members: [] };
    }
    busy = false;
  }

  async function select(reference: string, target: string) {
    try {
      session = await api.alignSelect(reference, target);
    } catch (e) {
      await run(() => Promise.reject(e));
    }
  }

  // Pick a member as the reference; keep a distinct target audible alongside it.
  async function setReference(m: AlignMember) {
    if (!session) return;
    let target = session.target;
    if (target === null || target === m.node_name) {
      target = session.members.find((x: AlignMember) => x.node_name !== m.node_name)?.node_name ?? m.node_name;
    }
    await select(m.node_name, target);
  }

  async function tune(m: AlignMember) {
    if (!session || session.reference === null || session.reference === m.node_name) return;
    await select(session.reference, m.node_name);
  }

  async function applyOffset(m: AlignMember, ms: number) {
    const clamped = Math.max(0, Math.min(sliderMax(m), Math.round(ms)));
    offsets[m.node_name] = clamped;
    try {
      if (m.kind === 'sendspin') await api.setSendspinDelay(m.node_name, clamped);
      else await api.setOutputLatency(m.node_name, clamped);
    } catch (e) {
      await run(() => Promise.reject(e));
    }
  }

  const groupTitle = $derived((session?.sources ?? []).map(label).join(' + '));
</script>

<div class="card info">
  <h2>Latency alignment</h2>
  <p class="card-sub">
    Speakers in a sync group can drift a few milliseconds apart. This tool plays an alternating
    <strong>tick-tock</strong> click on the group off one clock so you can align them <strong>by ear</strong>: pick one
    speaker as the reference, then tune each other speaker's delay until its clicks land exactly on the reference's. The
    two-tone alternation lets you tell a genuine match from being one full click out of step.
  </p>
</div>

{#if loading}
  <div class="card"><p class="empty">Loading…</p></div>
{:else if !active}
  <div class="card">
    <h2>Choose a group</h2>
    <p class="card-sub">
      A group is a source routed to two or more speakers that are on the network right now. Set routing up on the Routing
      tab first if nothing appears here.
    </p>
    {#if groups.length === 0}
      <p class="empty">No multi-speaker groups are active. Route one source to two or more present outputs to align them.</p>
    {:else}
      <ul class="group-list">
        {#each groups as g (g.sources.join('|'))}
          <li>
            <div class="grow">
              <div class="g-name">{g.sources.map(label).join(' + ')}</div>
              <div class="muted">{g.members.map((m) => label(m.node_name)).join(', ')}</div>
            </div>
            <button class="primary" disabled={busy || g.members.length < 2} onclick={() => start(g)}>
              {g.members.length < 2 ? 'Need 2+ speakers' : 'Start alignment'}
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{:else}
  <div class="card">
    <div class="row" style="justify-content:space-between; align-items:center">
      <h2 style="margin:0">Aligning: {groupTitle}</h2>
      <button class="danger" onclick={stop} disabled={busy}>Finish</button>
    </div>
    <p class="card-sub">
      The <strong>reference</strong> and the speaker you're <strong>tuning</strong> play together; everything else is
      muted. Nudge the tuned speaker until its clicks sit exactly on the reference's, then move to the next speaker.
      Because you can only add delay, make the physically latest speaker the reference.
    </p>

    <table>
      <thead>
        <tr><th>Speaker</th><th>Role</th><th>Offset</th><th></th></tr>
      </thead>
      <tbody>
        {#each session?.members ?? [] as m (m.node_name)}
          {@const isRef = session?.reference === m.node_name}
          {@const isTarget = session?.target === m.node_name}
          <tr class:audible={isRef || isTarget}>
            <td>
              {label(m.node_name)}
              <span class="badge">{m.kind === 'sendspin' ? 'Sendspin' : 'AirPlay'}</span>
            </td>
            <td>
              <label class="role">
                <input type="radio" name="ref" checked={isRef} onchange={() => setReference(m)} /> Reference
              </label>
              {#if isTarget}<span class="badge on">tuning</span>{/if}
            </td>
            <td>
              <div class="offset">
                <input
                  type="range"
                  min="0"
                  max={sliderMax(m)}
                  step={m.kind === 'sendspin' ? 5 : 10}
                  value={offsets[m.node_name] ?? 0}
                  disabled={!isTarget}
                  onchange={(e) => applyOffset(m, Number((e.currentTarget as HTMLInputElement).value))}
                />
                <span class="ms">{offsets[m.node_name] ?? 0} ms</span>
              </div>
              {#if isTarget}
                <div class="nudge">
                  <button onclick={() => applyOffset(m, (offsets[m.node_name] ?? 0) - 10)}>−10</button>
                  <button onclick={() => applyOffset(m, (offsets[m.node_name] ?? 0) - 1)}>−1</button>
                  <button onclick={() => applyOffset(m, (offsets[m.node_name] ?? 0) + 1)}>+1</button>
                  <button onclick={() => applyOffset(m, (offsets[m.node_name] ?? 0) + 10)}>+10</button>
                </div>
                {#if m.kind === 'raop'}
                  <p class="muted warn">Each change reloads this AirPlay sink — expect a brief gap before the click returns.</p>
                {/if}
              {/if}
            </td>
            <td style="text-align:right">
              {#if !isRef && !isTarget}
                <button onclick={() => tune(m)} disabled={busy}>Tune</button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}

<style>
  .group-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .group-list li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 12px;
    border: 1px solid var(--divider-color);
    border-radius: 8px;
  }
  .g-name {
    font-weight: 500;
  }
  tr.audible td {
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  .role {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    margin: 0;
  }
  .offset {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .offset input[type='range'] {
    flex: 1;
    min-width: 120px;
  }
  .ms {
    min-width: 64px;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .nudge {
    display: flex;
    gap: 4px;
    margin-top: 6px;
  }
  .nudge button {
    padding: 2px 8px;
  }
  .warn {
    font-size: 0.8rem;
    margin: 6px 0 0;
  }
</style>
