<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import type { NodeInfo, StatusInfo } from '../lib/types';
  import RoutingTab from './RoutingTab.svelte';

  // ---- Status + live graph ------------------------------------------------
  let status = $state<StatusInfo | null>(null);
  let nodes = $state<NodeInfo[]>([]);
  let loading = $state(true);
  let poll: ReturnType<typeof setInterval>;

  function fmtUptime(secs: number): string {
    const d = Math.floor(secs / 86400);
    const h = Math.floor((secs % 86400) / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = secs % 60;
    if (d) return `${d}d ${h}h ${m}m`;
    if (h) return `${h}h ${m}m`;
    if (m) return `${m}m ${s}s`;
    return `${s}s`;
  }

  async function refresh(silent = false) {
    if (!silent) loading = true;
    try {
      const [st, ns] = await Promise.all([api.status(), api.nodes().catch(() => ({ nodes: [], ports: [] }))]);
      status = st;
      nodes = ns.nodes.slice().sort((a, b) => a.node_id - b.node_id);
    } catch {
      if (!silent) {
        status = null;
        nodes = [];
      }
    } finally {
      if (!silent) loading = false;
    }
  }
  onMount(() => {
    refresh();
    // Keep the live graph reflecting changes made elsewhere while this tab is
    // open (a silent poll — no loading flicker). The routing matrix below is
    // already push-updated over the WebSocket.
    poll = setInterval(() => refresh(true), 4000);
  });
  onDestroy(() => clearInterval(poll));
</script>

<div class="card">
  <div class="row" style="justify-content:space-between; align-items:center">
    <h2 style="margin:0">Status</h2>
    <button class="ghost" onclick={() => refresh()} disabled={loading}>{loading ? 'Refreshing…' : 'Refresh'}</button>
  </div>
  {#if status}
    <dl class="status-grid">
      <div><dt>Version</dt><dd>{status.version}</dd></div>
      <div><dt>Uptime</dt><dd>{fmtUptime(status.uptime_secs)}</dd></div>
      <div>
        <dt>Discovery</dt>
        <dd><span class="badge {status.discovery_enabled ? 'on' : 'off'}">{status.discovery_enabled ? 'running' : 'off'}</span></dd>
      </div>
      <div><dt>PipeWire nodes</dt><dd>{status.pipewire_nodes}</dd></div>
      <div><dt>AirPlay outputs</dt><dd>{status.raop_outputs}</dd></div>
      <div><dt>Sendspin devices</dt><dd>{status.sendspin_devices}</dd></div>
      <div><dt>Routes</dt><dd>{status.routes}</dd></div>
    </dl>
  {:else if !loading}
    <p class="empty">Couldn't reach the daemon.</p>
  {/if}
  <p class="muted" style="font-size:0.8rem; margin:8px 0 0">
    Full logs are in Home Assistant → Settings → Add-ons → Audio Router → Log.
  </p>
</div>

<div class="card">
  <h2>PipeWire graph</h2>
  <p class="card-sub">Every node in the live audio graph right now — sources, sinks, and internal plumbing.</p>
  {#if nodes.length === 0}
    <p class="empty">{loading ? 'Loading…' : 'No nodes.'}</p>
  {:else}
    <div style="overflow-x:auto">
      <table>
        <thead><tr><th>ID</th><th>Node name</th><th>Media class</th></tr></thead>
        <tbody>
          {#each nodes as n (n.node_id)}
            <tr><td>{n.node_id}</td><td><code>{n.node_name}</code></td><td>{n.media_class ?? '—'}</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</div>

<div class="card">
  <h2>Routing matrix</h2>
  <p class="card-sub">
    The low-level source→output connections. Day-to-day you route <strong>music groups</strong> (Groups tab); this
    matrix is here to <strong>diagnose</strong> individual connections and see exactly what's wired — it updates live as
    you change things elsewhere.
  </p>
  <RoutingTab />
</div>

<div class="card info">
  <h2>Testing an output</h2>
  <p class="card-sub">
    Use the <strong>Play tone</strong> / <strong>Play announcement</strong> buttons on the Outputs tab to send a
    short diagnostic clip straight to a speaker and confirm it's alive and correctly wired. A full ducked
    announcement (with volume restore) is driven from Home Assistant, not here.
  </p>
</div>

<style>
  .status-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 12px;
    margin: 0;
  }
  .status-grid dt {
    color: var(--secondary-text-color);
    font-size: 0.8rem;
  }
  .status-grid dd {
    margin: 2px 0 0;
    font-size: 1.1rem;
    font-weight: 500;
  }
</style>
