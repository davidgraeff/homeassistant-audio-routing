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
  // Audible-member playback level (0–100), mirrored to the daemon.
  let level = $state(50);
  // Whether sendspin firmware applies a delay change live (Settings). When
  // false, a change restarts the group stream, so we don't stream during drag.
  let sendspinDelayLive = $state(false);

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
      const [st, gs, settings] = await Promise.all([
        api.alignStatus(),
        api.alignGroups(),
        api.settings().catch(() => null),
      ]);
      session = st;
      groups = gs;
      if (settings) sendspinDelayLive = settings.sendspin_delay_live;
      if (st.active) {
        level = st.volume;
        await seedOffsets();
      }
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
      level = session.volume;
      await seedOffsets();
    } catch (e) {
      await run(() => Promise.reject(e));
    }
    busy = false;
  }

  async function setLevel(v: number) {
    level = v;
    try {
      session = await api.alignVolume(v);
    } catch (e) {
      await run(() => Promise.reject(e));
    }
  }

  async function stop() {
    busy = true;
    if (await run(() => api.alignStop(), 'Alignment finished — volumes restored')) {
      session = { active: false, sources: [], reference: null, target: null, members: [], volume: level };
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

  // Live drag: sendspin delay is applied in-band and takes effect immediately,
  // so push while dragging (throttled). RAOP latency reloads the sink, so it's
  // only committed on release (onchange) — dragging just updates the readout.
  let throttleTimer: ReturnType<typeof setTimeout> | null = null;
  let pending: { m: AlignMember; ms: number } | null = null;
  function liveOffset(m: AlignMember, ms: number) {
    offsets[m.node_name] = ms; // immediate readout
    // Only stream while dragging when it actually takes effect live: sendspin
    // firmware that honors it. Otherwise (sendspin needing a restart, or RAOP)
    // we commit once on release.
    if (m.kind !== 'sendspin' || !sendspinDelayLive) return;
    pending = { m, ms };
    if (throttleTimer) return;
    throttleTimer = setTimeout(() => {
      throttleTimer = null;
      if (pending) {
        const p = pending;
        pending = null;
        void applyOffset(p.m, p.ms);
      }
    }, 100);
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

    <div class="field level">
      <label for="cal-vol">Playback volume: {level}%</label>
      <input
        id="cal-vol"
        type="range"
        min="0"
        max="100"
        step="5"
        value={level}
        oninput={(e) => (level = Number((e.currentTarget as HTMLInputElement).value))}
        onchange={(e) => setLevel(Number((e.currentTarget as HTMLInputElement).value))}
      />
      <p class="muted" style="font-size:0.8rem; margin:4px 0 0">Applies to the reference and the speaker being tuned.</p>
    </div>

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
                  oninput={(e) => liveOffset(m, Number((e.currentTarget as HTMLInputElement).value))}
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
                {:else if !sendspinDelayLive}
                  <p class="muted warn">Each change restarts the group stream — expect a brief gap before the click returns.</p>
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
  .level {
    max-width: 360px;
  }
</style>
