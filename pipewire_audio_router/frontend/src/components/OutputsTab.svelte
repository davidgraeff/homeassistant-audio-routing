<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { Encryption, OutputInfo } from '../lib/types';

  let outputs = $state<OutputInfo[]>([]);
  let loading = $state(true);
  let busy = $state(false);

  let name = $state('');
  let ip = $state('');
  let port = $state<number | ''>('');
  let encryption = $state<Encryption>('auth_setup');

  async function refresh() {
    loading = true;
    try {
      outputs = await api.outputs();
    } catch {
      outputs = [];
    }
    loading = false;
  }
  onMount(refresh);

  async function add(e: Event) {
    e.preventDefault();
    if (!name.trim() || !ip.trim()) return;
    busy = true;
    const ok = await run(
      () =>
        api.addOutput({
          name: name.trim(),
          ip: ip.trim(),
          port: port === '' ? undefined : Number(port),
          encryption,
        }),
      `Added output '${name.trim()}'`,
    );
    busy = false;
    if (ok) {
      name = '';
      ip = '';
      port = '';
      encryption = 'auth_setup';
      await refresh();
    }
  }

  async function remove(o: OutputInfo) {
    if (!confirm(`Remove RAOP output '${o.name}'? Its sink disappears from the graph.`)) return;
    if (await run(() => api.removeOutput(o.node_name), `Removed '${o.name}'`)) await refresh();
  }
</script>

<div class="card">
  <h2>RAOP (AirPlay) outputs</h2>
  <p class="card-sub">
    AV receivers this router streams out to. Added here are persisted and loaded live as
    <code>raop-sink</code> modules. Devices found via mDNS are loaded automatically and show up in the
    routing matrix without appearing in this list.
  </p>

  {#if loading}
    <p class="empty">Loading…</p>
  {:else if outputs.length === 0}
    <p class="empty">No configured outputs. Add one below, or rely on mDNS auto-discovery.</p>
  {:else}
    <div style="overflow-x:auto">
      <table>
        <thead>
          <tr><th>Name</th><th>IP</th><th>Port</th><th>Encryption</th><th>Status</th><th></th></tr>
        </thead>
        <tbody>
          {#each outputs as o (o.node_name)}
            <tr>
              <td>{o.name}</td>
              <td><code>{o.ip}</code></td>
              <td>{o.port}</td>
              <td>{o.encryption}</td>
              <td>
                <span class="badge {o.loaded ? 'on' : 'off'}">{o.loaded ? 'loaded' : 'not loaded'}</span>
              </td>
              <td style="text-align:right">
                <button class="danger" onclick={() => remove(o)}>Remove</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</div>

<div class="card">
  <h2>Add an output</h2>
  <form onsubmit={add}>
    <div class="row">
      <div class="grow field">
        <label for="o-name">Name</label>
        <input id="o-name" type="text" bind:value={name} placeholder="Living Room" required />
      </div>
      <div class="grow field">
        <label for="o-ip">IP address</label>
        <input id="o-ip" type="text" bind:value={ip} placeholder="192.168.1.50" required />
      </div>
      <div class="field" style="flex:0 0 90px">
        <label for="o-port">Port</label>
        <input id="o-port" type="number" min="1" max="65535" bind:value={port} placeholder="7000" />
      </div>
      <div class="field" style="flex:0 0 140px">
        <label for="o-enc">Encryption</label>
        <select id="o-enc" bind:value={encryption}>
          <option value="auth_setup">auth_setup</option>
          <option value="RSA">RSA</option>
          <option value="none">none</option>
        </select>
      </div>
      <div class="field">
        <button class="primary" type="submit" disabled={busy || !name.trim() || !ip.trim()}>Add</button>
      </div>
    </div>
    <p class="muted" style="font-size:0.8rem; margin:0">
      Port defaults to 7000, encryption to <code>auth_setup</code> — the mode proven against real hardware.
    </p>
  </form>
</div>
