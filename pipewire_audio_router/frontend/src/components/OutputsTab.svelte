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
    if (!confirm(`Remove AirPlay output '${o.name}'? It disappears from the routing matrix (an auto-discovered one reappears when the device next announces).`))
      return;
    if (await run(() => api.removeOutput(o.node_name), `Removed '${o.name}'`)) await refresh();
  }
</script>

<div class="card info">
  <h2>Supported outputs</h2>
  <p class="card-sub">
    This router can stream to <strong>AirPlay (RAOP)</strong> receivers (AV receivers, HomePods, AirPlay
    speakers) and <strong>Sendspin</strong> speakers — the open multi-room protocol used by ESPHome and
    Home Assistant Voice PE. Compatible devices on your network are discovered automatically and appear
    below and in the routing matrix; you don't need to configure anything. Route one source to several
    Sendspin devices and they play in a synchronized group.
  </p>
</div>

<div class="card">
  <h2>Outputs</h2>
  <p class="card-sub">
    Everything this router can send audio to right now. Auto-discovered devices are tagged
    <span class="badge auto">auto</span>; offline entries are ones with saved routing that aren't
    currently on the network.
  </p>

  {#if loading}
    <p class="empty">Loading…</p>
  {:else if outputs.length === 0}
    <p class="empty">
      No outputs yet. Compatible AirPlay / Sendspin devices appear here automatically when they're on the
      network — or add an AirPlay receiver manually below.
    </p>
  {:else}
    <div style="overflow-x:auto">
      <table>
        <thead>
          <tr><th>Name</th><th>Type</th><th>IP</th><th>Port</th><th>Encryption</th><th>Status</th><th></th></tr>
        </thead>
        <tbody>
          {#each outputs as o (o.node_name)}
            <tr class:offline={!o.present}>
              <td>
                {o.name}
                {#if !o.configured}<span class="badge auto" title="Found via mDNS auto-discovery">auto</span>{/if}
              </td>
              <td>{o.kind === 'sendspin' ? 'Sendspin' : 'AirPlay'}</td>
              <td>{#if o.ip}<code>{o.ip}</code>{:else}—{/if}</td>
              <td>{o.port ?? '—'}</td>
              <td>{o.encryption ?? '—'}</td>
              <td><span class="badge {o.present ? 'on' : 'off'}">{o.present ? 'online' : 'offline'}</span></td>
              <td style="text-align:right">
                {#if o.kind === 'airplay'}
                  <button class="danger" onclick={() => remove(o)}>Remove</button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/if}

  <h2>Add an AirPlay output</h2>
  <p class="card-sub">
    Optional — most receivers are found automatically. Add one manually to pin a specific IP or
    encryption mode (Sendspin devices can't be added by hand; they're always auto-discovered).
  </p>
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
