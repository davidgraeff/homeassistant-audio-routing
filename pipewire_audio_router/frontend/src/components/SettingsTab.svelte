<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';

  let loading = $state(true);

  // General settings (settings_store.rs).
  let duck = $state(0.25);
  let discovery = $state(true);
  // Blank = module default (1500 ms) → sent as null.
  let raopLatency = $state<number | ''>('');
  let generalBusy = $state(false);
  let discoveryBusy = $state(false);

  // Sync (moved here from the Outputs tab): daemon-wide presentation lead.
  let groupLeadMs = $state<number | ''>('');
  let syncBusy = $state(false);
  // Whether sendspin delay changes apply to the running stream (future firmware)
  // or need a stream restart (current ESPHome firmware).
  let sendspinDelayLive = $state(false);
  let delayModeBusy = $state(false);

  async function refresh() {
    loading = true;
    try {
      const [s, sync] = await Promise.all([api.settings(), api.syncSettings().catch(() => null)]);
      duck = s.default_duck;
      discovery = s.discovery_enabled;
      raopLatency = s.default_raop_latency_ms ?? '';
      sendspinDelayLive = s.sendspin_delay_live;
      if (sync) groupLeadMs = sync.group_lead_ms;
    } catch {
      // leave defaults; a toast already surfaced the error via run() elsewhere
    }
    loading = false;
  }
  onMount(refresh);

  async function saveGeneral() {
    generalBusy = true;
    await run(
      () => api.setSettings({ default_duck: duck, default_raop_latency_ms: raopLatency === '' ? null : Number(raopLatency) }),
      'Settings saved',
    );
    generalBusy = false;
  }

  // Discovery toggles immediately (it's a live on/off), not batched with Save.
  async function toggleDiscovery() {
    discoveryBusy = true;
    const next = !discovery;
    if (await run(() => api.setSettings({ discovery_enabled: next }), `Discovery ${next ? 'enabled' : 'disabled'}`)) {
      discovery = next;
    }
    discoveryBusy = false;
  }

  async function saveGroupLead() {
    if (groupLeadMs === '' || Number(groupLeadMs) < 0) return;
    syncBusy = true;
    await run(() => api.setGroupLead(Number(groupLeadMs)), `Group lead set to ${groupLeadMs} ms`);
    syncBusy = false;
  }

  async function toggleDelayLive() {
    delayModeBusy = true;
    const next = !sendspinDelayLive;
    if (await run(() => api.setSettings({ sendspin_delay_live: next }), 'Setting saved')) {
      sendspinDelayLive = next;
    }
    delayModeBusy = false;
  }
</script>

{#if loading}
  <div class="card"><p class="empty">Loading…</p></div>
{:else}
  <div class="card">
    <h2>General</h2>
    <p class="card-sub">Daemon-wide defaults. Per-device tuning (output latency, sendspin delay) lives on the Outputs tab.</p>

    <div class="field">
      <label for="set-duck">Default duck level: {duck.toFixed(2)}</label>
      <input id="set-duck" type="range" min="0" max="1" step="0.05" bind:value={duck} />
      <p class="muted" style="font-size:0.8rem; margin:4px 0 0">
        How far playing sources are lowered during an announcement that doesn't request its own level. Home Assistant's
        announcements pass their own value, so this mainly affects the Diagnostics test tool.
      </p>
    </div>

    <div class="field">
      <label for="set-raop-latency">Default AirPlay latency for new outputs (ms)</label>
      <input id="set-raop-latency" type="number" min="0" max="5000" step="10" bind:value={raopLatency} placeholder="1500 (module default)" />
      <p class="muted" style="font-size:0.8rem; margin:4px 0 0">
        Stamped onto a newly-added AirPlay output that doesn't set its own latency. Leave blank to keep the receiver's
        default (~1500 ms). Existing outputs are unaffected.
      </p>
    </div>

    <button class="primary" onclick={saveGeneral} disabled={generalBusy}>Save</button>
  </div>

  <div class="card">
    <h2>Auto-discovery</h2>
    <p class="card-sub">
      Finds AirPlay and Sendspin devices on your network via mDNS and lists them automatically. Turning this off stops
      discovering <strong>new</strong> devices; ones already present stay until they drop off the network on their own.
    </p>
    <label class="check">
      <input type="checkbox" checked={discovery} disabled={discoveryBusy} onchange={toggleDiscovery} />
      mDNS discovery {discovery ? 'enabled' : 'disabled'}
    </label>
  </div>

  <div class="card">
    <h2>Group sync</h2>
    <p class="card-sub">
      When several outputs are routed from the same source they form a sync group off one clock. The
      <strong>group lead</strong> is how far ahead audio is scheduled — every member must buffer within it, so raise it to
      keep the slowest member in step (an AirPlay receiver can buffer up to ~1500 ms), or lower it for a snappier start.
      Fine-tune an individual speaker with its per-output value on the Outputs tab.
    </p>
    <div class="row">
      <div class="field" style="flex:0 0 160px">
        <label for="group-lead">Group lead (ms)</label>
        <input id="group-lead" type="number" min="0" max="5000" step="10" bind:value={groupLeadMs} placeholder="250" />
      </div>
      <div class="field">
        <button class="primary" onclick={saveGroupLead} disabled={syncBusy || groupLeadMs === ''}>Apply</button>
      </div>
    </div>

    <label class="check" style="margin-top:12px">
      <input type="checkbox" checked={sendspinDelayLive} disabled={delayModeBusy} onchange={toggleDelayLive} />
      Sendspin speakers apply delay changes live
    </label>
    <p class="muted" style="font-size:0.8rem; margin:4px 0 0">
      Leave off for current ESPHome firmware, which only picks up a per-speaker delay at stream start — so changing one
      briefly restarts the group's stream (like an AirPlay reload). Turn on only for firmware that applies
      <code>SetStaticDelay</code> to the running stream, to avoid the restart.
    </p>
  </div>
{/if}
