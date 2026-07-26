<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import type { NodeInfo, OutputInfo, StatusInfo } from '../lib/types';
  import FlowGraph from './FlowGraph.svelte';

  // PipeWire internals that are never routing endpoints — shown grayed and
  // non-interactive in the raw node list.
  const INTERNAL = /(Dummy-Driver|Freewheel-Driver)/i;
  const isInternal = (n: NodeInfo) => n.media_class == null || INTERNAL.test(n.node_name);

  // ---- Status + live graph ------------------------------------------------
  let status = $state<StatusInfo | null>(null);
  let nodes = $state<NodeInfo[]>([]);
  // AirPlay-2 receivers with decoded capabilities, for the capability card below.
  let ap2 = $state<OutputInfo[]>([]);
  let loading = $state(true);
  let poll: ReturnType<typeof setInterval>;

  // Live PTP-lock label for the capability table. A lock isn't needed for
  // single-room realtime playback (the receiver free-runs off our PT=87
  // anchors), so an unlocked lone receiver is neutral, not an error.
  function lockLabel(o: OutputInfo): { cls: string; text: string } {
    if (!o.present) return { cls: 'badge off', text: 'offline' };
    if (o.ptp_locked) return { cls: 'badge on', text: 'locked' };
    if (o.ptp_relevant) return { cls: 'badge warn', text: 'unlocked (drift risk)' };
    return { cls: 'badge', text: 'unlocked (single-room ok)' };
  }

  // Map the host verdict to an existing badge class:
  //   adequate => on (green), marginal => neutral, underpowered => warn (red).
  function verdictBadge(v: string): string {
    if (v === 'adequate') return 'badge on';
    if (v === 'underpowered') return 'badge warn';
    return 'badge';
  }

  function fmtRam(mb: number): string {
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GiB`;
    return `${mb} MiB`;
  }

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
      const [st, ns, outs] = await Promise.all([
        api.status(),
        api.nodes().catch(() => ({ nodes: [], ports: [] })),
        api.outputs().catch(() => [] as OutputInfo[]),
      ]);
      status = st;
      nodes = ns.nodes.slice().sort((a, b) => a.node_id - b.node_id);
      ap2 = outs.filter((o) => o.kind === 'airplay2');
    } catch {
      if (!silent) {
        status = null;
        nodes = [];
        ap2 = [];
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

{#if status?.host}
  {@const host = status.host}
  <div class="card">
    <div class="row" style="justify-content:space-between; align-items:center">
      <h2 style="margin:0">Host capability</h2>
      <span class={verdictBadge(host.verdict)}>{host.verdict}</span>
    </div>
    <p class="card-sub" style="margin-top:6px">Is this machine strong enough for realtime multi-room audio?</p>
    <dl class="status-grid">
      <div><dt>CPU</dt><dd style="font-size:1rem">{host.cpu_model}</dd></div>
      <div><dt>Cores</dt><dd>{host.cores}</dd></div>
      <div><dt>Architecture</dt><dd style="font-size:1rem">{host.arch}</dd></div>
      <div><dt>RAM</dt><dd>{fmtRam(host.mem_total_mb)}</dd></div>
      <div>
        <dt>Realtime scheduling</dt>
        <dd><span class="badge {host.rt_available ? 'on' : 'warn'}">{host.rt_available ? 'yes' : 'no'}</span></dd>
      </div>
    </dl>
    <p class="muted" style="font-size:0.85rem; margin:10px 0 0">{host.note}</p>
  </div>
{/if}

{#if ap2.length}
  <div class="card">
    <h2 style="margin:0">AirPlay-2 receiver capabilities</h2>
    <p class="card-sub" style="margin-top:6px">
      Decoded from each receiver's mDNS <code>features</code> bitmask. We stream realtime ALAC, so a live PTP lock is only
      needed to keep <em>multiple</em> rooms in sync — a lone receiver plays fine unlocked.
    </p>
    <div style="overflow-x:auto">
      <table>
        <thead>
          <tr><th>Receiver</th><th>PTP (bit 41)</th><th>Buffered audio (bit 40)</th><th>Transient pairing (bit 48)</th><th>PTP lock (live)</th><th>features</th></tr>
        </thead>
        <tbody>
          {#each ap2 as o (o.node_name)}
            {@const lock = lockLabel(o)}
            <tr class:internal={!o.present}>
              <td>{o.name}</td>
              {#if o.ap2_features}
                <td><span class="badge {o.ap2_features.ptp ? 'on' : ''}">{o.ap2_features.ptp ? 'yes' : 'no'}</span></td>
                <td><span class="badge {o.ap2_features.buffered_audio ? 'on' : ''}">{o.ap2_features.buffered_audio ? 'yes' : 'no'}</span></td>
                <td><span class="badge {o.ap2_features.transient_pairing ? 'on' : ''}">{o.ap2_features.transient_pairing ? 'yes' : 'no'}</span></td>
                <td><span class={lock.cls}>{lock.text}</span></td>
                <td><code>{o.ap2_features.raw}</code></td>
              {:else}
                <td colspan="3" class="muted">features not advertised / not yet seen</td>
                <td><span class={lock.cls}>{lock.text}</span></td>
                <td>—</td>
              {/if}
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </div>
{/if}

<FlowGraph />

<div class="card">
  <details>
    <summary><h2 style="display:inline; margin:0">Raw PipeWire graph</h2></summary>
    <p class="card-sub">
      Every node in the live audio graph right now — sources, sinks, and internal plumbing. Grayed rows are internal
      nodes (drivers, unclassified) that aren't routing endpoints; they're shown for diagnostics only.
    </p>
    {#if nodes.length === 0}
      <p class="empty">{loading ? 'Loading…' : 'No nodes.'}</p>
    {:else}
      <div style="overflow-x:auto">
        <table>
          <thead><tr><th>ID</th><th>Node name</th><th>Media class</th></tr></thead>
          <tbody>
            {#each nodes as n (n.node_id)}
              <tr class:internal={isInternal(n)}><td>{n.node_id}</td><td><code>{n.node_name}</code></td><td>{n.media_class ?? '—'}</td></tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </details>
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
  summary {
    cursor: pointer;
    list-style-position: outside;
  }
  tr.internal {
    opacity: 0.45;
  }
</style>
