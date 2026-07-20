<script lang="ts">
  import { onMount } from 'svelte';
  import { routing } from '../lib/routing';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { NodeInfo, StatusInfo } from '../lib/types';

  // ---- Status + live graph ------------------------------------------------
  let status = $state<StatusInfo | null>(null);
  let nodes = $state<NodeInfo[]>([]);
  let loading = $state(true);

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

  async function refresh() {
    loading = true;
    try {
      const [st, ns] = await Promise.all([api.status(), api.nodes().catch(() => ({ nodes: [], ports: [] }))]);
      status = st;
      nodes = ns.nodes.slice().sort((a, b) => a.node_id - b.node_id);
    } catch {
      status = null;
      nodes = [];
    }
    loading = false;
  }
  onMount(refresh);

  // ---- Announce test tool (relocated from the former Announce tab) --------
  // node-id based: the daemon ducks live sources into a real sink and plays a
  // clip, so only outputs with a live node_id are valid targets — this excludes
  // virtual sendspin devices and offline outputs (both node_id: null).
  let target = $state<number | null>(null);
  let mode = $state<'url' | 'wyoming'>('url');
  let duck = $state(0.25);
  let busy = $state(false);
  let url = $state('');
  let host = $state('');
  let port = $state<number | ''>('');
  let text = $state('');
  let voice = $state('');

  const announceable = $derived($routing.matrix.outputs.filter((o) => o.node_id != null));

  $effect(() => {
    if ((target === null || !announceable.some((o) => o.node_id === target)) && announceable.length > 0) {
      target = announceable[0].node_id;
    }
  });

  async function send(e: Event) {
    e.preventDefault();
    if (target === null) return;
    busy = true;
    if (mode === 'url') {
      await run(() => api.announceUrl(target!, url.trim(), duck), 'Announcement played');
    } else {
      await run(
        () =>
          api.announceWyoming(
            target!,
            { host: host.trim(), port: port === '' ? undefined : Number(port), text: text.trim(), voice: voice.trim() || null },
            duck,
          ),
        'Announcement played',
      );
    }
    busy = false;
  }
</script>

<div class="card">
  <div class="row" style="justify-content:space-between; align-items:center">
    <h2 style="margin:0">Status</h2>
    <button class="ghost" onclick={refresh} disabled={loading}>{loading ? 'Refreshing…' : 'Refresh'}</button>
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
  <h2>Test announcement</h2>
  <p class="card-sub">
    Diagnostic tool: ducks every source currently feeding the chosen output, plays a clip, then restores volumes. Blocks
    until playback finishes. Can only target a live AirPlay/PipeWire sink — virtual Sendspin devices aren't addressable
    this way (test those via a Home Assistant media player).
  </p>

  {#if announceable.length === 0}
    <p class="empty">No outputs available to announce to.</p>
  {:else}
    <form onsubmit={send}>
      <div class="field">
        <label for="an-target">Output</label>
        <select id="an-target" bind:value={target}>
          {#each announceable as o (o.node_name)}
            <option value={o.node_id}>{o.display_name}</option>
          {/each}
        </select>
      </div>

      <div class="field">
        <span class="group-label">Source</span>
        <div class="row" style="gap:16px">
          <label style="display:flex; gap:6px; align-items:center; margin:0">
            <input type="radio" value="url" bind:group={mode} /> URL (fetched + decoded)
          </label>
          <label style="display:flex; gap:6px; align-items:center; margin:0">
            <input type="radio" value="wyoming" bind:group={mode} /> Wyoming TTS
          </label>
        </div>
      </div>

      {#if mode === 'url'}
        <div class="field">
          <label for="an-url">Audio URL</label>
          <input id="an-url" type="url" bind:value={url} placeholder="http://homeassistant.local:8123/api/tts_proxy/….mp3" />
        </div>
      {:else}
        <div class="row">
          <div class="grow field">
            <label for="an-host">Wyoming host</label>
            <input id="an-host" type="text" bind:value={host} placeholder="192.168.1.20" />
          </div>
          <div class="field" style="flex:0 0 100px">
            <label for="an-port">Port</label>
            <input id="an-port" type="number" bind:value={port} placeholder="10200" />
          </div>
          <div class="field" style="flex:0 0 140px">
            <label for="an-voice">Voice (optional)</label>
            <input id="an-voice" type="text" bind:value={voice} placeholder="default" />
          </div>
        </div>
        <div class="field">
          <label for="an-text">Text</label>
          <textarea id="an-text" rows="2" bind:value={text} placeholder="Front door opened"></textarea>
        </div>
      {/if}

      <div class="field">
        <label for="an-duck">Duck volume: {duck.toFixed(2)}</label>
        <input id="an-duck" type="range" min="0" max="1" step="0.05" bind:value={duck} />
      </div>

      <button class="primary" type="submit" disabled={busy || target === null}>
        {busy ? 'Announcing…' : 'Send test announcement'}
      </button>
    </form>
  {/if}
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
