<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { routing, isLinked } from '../lib/routing';
  import { api } from '../lib/api';
  import { run, toast } from '../lib/toast';
  import type { MediaPlayerInfo } from '../lib/types';

  let volumes = $state<Record<number, number>>({});
  let pending = $state<Set<string>>(new Set());
  let poll: ReturnType<typeof setInterval>;

  async function refreshVolumes() {
    try {
      const players: MediaPlayerInfo[] = await api.mediaPlayers();
      const next: Record<number, number> = {};
      for (const p of players) if (typeof p.volume === 'number') next[p.node_id] = p.volume;
      volumes = next;
    } catch {
      /* keep last-known volumes */
    }
  }

  onMount(() => {
    refreshVolumes();
    poll = setInterval(refreshVolumes, 3000);
  });
  onDestroy(() => clearInterval(poll));

  async function toggle(sourceId: number, outputId: number, linked: boolean) {
    const key = `${sourceId}:${outputId}`;
    pending = new Set(pending).add(key);
    await run(() => (linked ? api.unlink(sourceId, outputId) : api.link(sourceId, outputId)));
    const next = new Set(pending);
    next.delete(key);
    pending = next;
  }

  async function onVolume(nodeId: number, value: number) {
    volumes = { ...volumes, [nodeId]: value };
    try {
      await api.setVolume(nodeId, value);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
</script>

<div class="card">
  <h2>Routing matrix</h2>
  <p class="card-sub">Click a cell to link or unlink a source and an output. Live-updated from PipeWire.</p>

  {#if $routing.matrix.outputs.length === 0}
    <p class="empty">No outputs available yet — add one under the Outputs tab.</p>
  {:else if $routing.matrix.sources.length === 0}
    <p class="empty">No sources present right now (nothing is playing into the router).</p>
  {:else}
    <div style="overflow-x:auto">
      <table class="matrix">
        <thead>
          <tr>
            <th class="corner">Source ↓ / Output →</th>
            {#each $routing.matrix.outputs as o (o.node_name)}
              <th class="out">
                <div class="out-head">
                  <span>{o.display_name}</span>
                  <input
                    type="range"
                    min="0"
                    max="1"
                    step="0.01"
                    value={volumes[o.node_id] ?? 1}
                    oninput={(e) => onVolume(o.node_id, parseFloat(e.currentTarget.value))}
                    title="Volume"
                  />
                </div>
              </th>
            {/each}
          </tr>
        </thead>
        <tbody>
          {#each $routing.matrix.sources as s (s.node_name)}
            <tr>
              <th class="src">{s.display_name}</th>
              {#each $routing.matrix.outputs as o (o.node_name)}
                {@const linked = isLinked($routing.matrix, s.node_id, o.node_id)}
                {@const key = `${s.node_id}:${o.node_id}`}
                <td
                  class="cell"
                  class:linked
                  onclick={() => toggle(s.node_id, o.node_id, linked)}
                >
                  {pending.has(key) ? '…' : linked ? '✓' : '—'}
                </td>
              {/each}
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</div>

<style>
  .matrix th.corner,
  .matrix th.src {
    text-align: left;
    white-space: nowrap;
  }
  .out {
    text-align: center;
    min-width: 130px;
  }
  .out-head {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: center;
  }
  .cell {
    text-align: center;
    cursor: pointer;
    user-select: none;
    font-size: 1.1rem;
    color: var(--secondary-text-color);
  }
  .cell:hover {
    background: color-mix(in srgb, var(--primary-color) 12%, transparent);
  }
  .cell.linked {
    color: var(--primary-color);
    font-weight: 600;
  }
</style>
