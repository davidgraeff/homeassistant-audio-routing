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

  // Per-device sync tuning: static delays (sendspin) and per-output receiver
  // latency (RAOP). The daemon-wide group lead lives on the Settings tab.
  // Per-row editable sync value (ms): RAOP latency or sendspin static delay.
  let edit = $state<Record<string, number | ''>>({});

  async function refresh() {
    loading = true;
    try {
      const [outs, delays] = await Promise.all([
        api.outputs(),
        api.sendspinDelays().catch(() => ({}) as Record<string, number>),
      ]);
      outputs = outs;
      // Seed the editable sync fields from current state.
      const next: Record<string, number | ''> = {};
      for (const o of outs) {
        if (o.kind === 'sendspin') next[o.node_name] = delays[o.node_name] ?? 0;
        else next[o.node_name] = o.latency_ms ?? '';
      }
      edit = next;
    } catch {
      outputs = [];
    }
    loading = false;
  }
  onMount(refresh);

  // Apply the per-row sync value: RAOP → receiver latency, sendspin → static delay.
  async function applySync(o: OutputInfo) {
    const v = edit[o.node_name];
    if (o.kind === 'sendspin') {
      const ms = v === '' ? 0 : Number(v);
      if (await run(() => api.setSendspinDelay(o.node_name, ms), `Set '${o.name}' delay to ${ms} ms`))
        await refresh();
    } else {
      const ms = v === '' ? null : Number(v);
      if (await run(() => api.setOutputLatency(o.node_name, ms), `Set '${o.name}' latency`)) await refresh();
    }
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
    if (!confirm(`Remove AirPlay output '${o.name}'? It disappears from the routing matrix (an auto-discovered one reappears when the device next announces).`))
      return;
    if (await run(() => api.removeOutput(o.node_name), `Removed '${o.name}'`)) await refresh();
  }

  // Configure modal for a manually-added output (IP / port / encryption).
  let configuring = $state<OutputInfo | null>(null);
  let cfgIp = $state('');
  let cfgPort = $state<number | ''>('');
  let cfgEnc = $state<Encryption>('auth_setup');
  let cfgBusy = $state(false);

  function openConfigure(o: OutputInfo) {
    configuring = o;
    cfgIp = o.ip ?? '';
    cfgPort = o.port ?? '';
    cfgEnc = (o.encryption as Encryption) ?? 'auth_setup';
  }

  async function saveConfigure(e: Event) {
    e.preventDefault();
    if (!configuring || !cfgIp.trim() || cfgPort === '') return;
    cfgBusy = true;
    const target = configuring;
    const ok = await run(
      () => api.configureOutput(target.node_name, { ip: cfgIp.trim(), port: Number(cfgPort), encryption: cfgEnc }),
      `Reconfigured '${target.name}'`,
    );
    cfgBusy = false;
    if (ok) {
      configuring = null;
      await refresh();
    }
  }
</script>

<div class="card info">
  <h2>Supported outputs</h2>
  <p class="card-sub">
    This router can stream to <strong>AirPlay (RAOP)</strong> receivers (AV receivers, HomePods, AirPlay
    speakers) and <strong>Sendspin</strong> speakers — the open multi-room protocol used by ESPHome and
    Home Assistant Voice PE. Compatible devices on your network are discovered automatically and appear
    below and in the routing matrix; you don't need to configure anything. Route one source to several
    Sendspin devices — or a mix of Sendspin and AirPlay — and they play in one synchronized group.
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
          <tr><th>Name</th><th>Type</th><th>IP</th><th>Port</th><th>Encryption</th><th>Sync offset</th><th>Status</th><th></th></tr>
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
              <td>
                {#if o.kind === 'sendspin'}
                  <div class="sync-cell">
                    <input
                      type="number"
                      min="0"
                      max="5000"
                      step="10"
                      bind:value={edit[o.node_name]}
                      title="Static delay in ms (0 = none)"
                    />
                    <button onclick={() => applySync(o)} title="Apply static delay">Set</button>
                  </div>
                {:else}
                  <div class="sync-cell">
                    <input
                      type="number"
                      min="0"
                      max="5000"
                      step="10"
                      bind:value={edit[o.node_name]}
                      placeholder="1500"
                      title="RAOP receiver latency in ms (blank = module default 1500)"
                    />
                    <button onclick={() => applySync(o)} title="Apply latency (reloads the sink)">Set</button>
                  </div>
                {/if}
              </td>
              <td><span class="badge {o.present ? 'on' : 'off'}">{o.present ? 'online' : 'offline'}</span></td>
              <td style="text-align:right; white-space:nowrap">
                {#if o.kind === 'airplay'}
                  {#if o.configured}
                    <button class="ghost" onclick={() => openConfigure(o)}>Configure</button>
                  {/if}
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

{#if configuring}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="modal-backdrop" onclick={() => (configuring = null)}>
    <div class="modal-card card" onclick={(e) => e.stopPropagation()}>
      <div class="card-head">
        <h2>Configure '{configuring.name}'</h2>
        <button class="ghost" type="button" onclick={() => (configuring = null)}>Close</button>
      </div>
      <p class="card-sub">
        Connection settings for this manually-added AirPlay receiver. Saving reloads its sink so the change
        takes effect immediately. To rename it, remove and re-add it (the name is its identity).
      </p>
      <form onsubmit={saveConfigure}>
        <div class="row">
          <div class="grow field">
            <label for="cfg-ip">IP address</label>
            <input id="cfg-ip" type="text" bind:value={cfgIp} placeholder="192.168.1.50" required />
          </div>
          <div class="field" style="flex:0 0 100px">
            <label for="cfg-port">Port</label>
            <input id="cfg-port" type="number" min="1" max="65535" bind:value={cfgPort} placeholder="7000" required />
          </div>
          <div class="field" style="flex:0 0 150px">
            <label for="cfg-enc">Encryption</label>
            <select id="cfg-enc" bind:value={cfgEnc}>
              <option value="auth_setup">auth_setup</option>
              <option value="RSA">RSA</option>
              <option value="none">none</option>
            </select>
          </div>
        </div>
        <div class="row" style="justify-content:flex-end">
          <button type="button" class="ghost" onclick={() => (configuring = null)}>Cancel</button>
          <button class="primary" type="submit" disabled={cfgBusy || !cfgIp.trim() || cfgPort === ''}>Save</button>
        </div>
      </form>
    </div>
  </div>
{/if}

<style>
  tr.offline td {
    opacity: 0.55;
  }
  .badge.auto {
    background: color-mix(in srgb, var(--primary-color) 18%, transparent);
    color: var(--primary-color);
    margin-left: 6px;
  }
  .sync-cell {
    display: flex;
    gap: 4px;
    align-items: center;
  }
  .sync-cell input {
    width: 74px;
  }
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding: 6vh 1rem 1rem;
    z-index: 50;
  }
  .modal-card {
    width: min(560px, 100%);
    max-height: 84vh;
    overflow: auto;
    margin: 0;
  }
  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
</style>
