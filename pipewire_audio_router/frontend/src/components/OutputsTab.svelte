<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { Encryption, OutputInfo, SendspinInfo } from '../lib/types';

  let outputs = $state<OutputInfo[]>([]);
  let loading = $state(true);
  let busy = $state(false);

  let name = $state('');
  let ip = $state('');
  let port = $state<number | ''>('');
  let encryption = $state<Encryption>('auth_setup');

  let sendspin = $state<SendspinInfo[]>([]);
  let newSendspin = $state('');

  async function refresh() {
    loading = true;
    try {
      outputs = await api.outputs();
    } catch {
      outputs = [];
    }
    try {
      sendspin = await api.sendspin();
    } catch {
      sendspin = [];
    }
    loading = false;
  }
  onMount(refresh);

  async function addSendspin(e: Event) {
    e.preventDefault();
    if (!newSendspin.trim()) return;
    busy = true;
    const ok = await run(() => api.addSendspin(newSendspin.trim()), `Added sendspin output '${newSendspin.trim()}'`);
    busy = false;
    if (ok) {
      newSendspin = '';
      await refresh();
    }
  }

  async function removeSendspin(s: SendspinInfo) {
    if (!confirm(`Remove sendspin output '${s.name}'?`)) return;
    if (await run(() => api.removeSendspin(s.node_name), `Removed '${s.name}'`)) await refresh();
  }

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
    if (!confirm(`Remove AirPlay output '${o.name}'? Its sink disappears from the graph.`)) return;
    if (await run(() => api.removeOutput(o.node_name), `Removed '${o.name}'`)) await refresh();
  }
</script>

<div class="card">
  <h2>AirPlay outputs</h2>
  <p class="card-sub">
   AirPlay is Apple’s wireless technology that allows you to stream music to AirPlay capable devices.
  </p>
  {#if loading}
    <p class="empty">Loading…</p>
  {:else if outputs.length === 0}
    <p class="empty">No outputs found yet. Add one manually below.</p>
  {:else}
    <div style="overflow-x:auto">
      <table>
        <thead>
          <tr><th>Name</th><th>IP</th><th>Port</th><th>Encryption</th><th>Status</th><th></th></tr>
        </thead>
        <tbody>
          {#each outputs as o (o.node_name)}
            <tr class:offline={!o.present}>
              <td>
                {o.name}
                {#if !o.configured}<span class="badge auto" title="Found via mDNS auto-discovery">auto</span>{/if}
              </td>
              <td>{#if o.ip}<code>{o.ip}</code>{:else}—{/if}</td>
              <td>{o.port ?? '—'}</td>
              <td>{o.encryption ?? '—'}</td>
              <td><span class="badge {o.present ? 'on' : 'off'}">{o.present ? 'online' : 'offline'}</span></td>
              <td style="text-align:right">
                <button class="danger" onclick={() => remove(o)}>Remove</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

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

<div class="card">
  <h2>Sendspin outputs</h2>
  <p class="card-sub">
    Sendspin is an open-source, license-free multi-room audio and media synchronization protocol developed by the Open Home Foundation.
  </p>
  {#if sendspin.length === 0}
    <p class="empty">No sendspin outputs configured.</p>
  {:else}
    <table>
      <thead><tr><th>Name</th><th>Port</th><th>Status</th><th></th></tr></thead>
      <tbody>
        {#each sendspin as s (s.node_name)}
          <tr>
            <td>{s.name}</td>
            <td>{s.port}</td>
            <td><span class="badge {s.running ? 'on' : 'off'}">{s.running ? 'running' : 'stopped'}</span></td>
            <td style="text-align:right"><button class="danger" onclick={() => removeSendspin(s)}>Remove</button></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
  <form onsubmit={addSendspin} style="margin-top:12px">
    <div class="row">
      <div class="grow field">
        <label for="ss-name">New sendspin output name</label>
        <input id="ss-name" type="text" bind:value={newSendspin} placeholder="Kitchen" />
      </div>
      <div class="field"><button class="primary" type="submit" disabled={busy || !newSendspin.trim()}>Add</button></div>
    </div>
  </form>
</div>

<style>
  tr.offline td {
    opacity: 0.55;
  }
  .badge.auto {
    background: color-mix(in srgb, var(--primary-color) 18%, transparent);
    color: var(--primary-color);
    margin-left: 6px;
  }
</style>
