<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run, toast } from '../lib/toast';
  import type { LeadFloorSource } from '../lib/types';

  let loading = $state(true);

  // General settings (settings_store.rs).
  let duck = $state(0.25);
  let discovery = $state(true);
  let generalBusy = $state(false);
  let discoveryBusy = $state(false);

  // Sync (moved here from the Outputs tab): daemon-wide presentation lead.
  let groupLeadMs = $state<number | ''>('');
  let syncBusy = $state(false);
  // The daemon raises every group's send-ahead to the largest buffer a member asked
  // for (`min_buffer_ms` + its static delay), because the protocol makes that
  // mandatory — so a lower value here simply has no effect. Kept in state so the input
  // can enforce it as its minimum and the copy can say which speaker set it.
  let leadFloorMs = $state(0);
  let leadEffectiveMs = $state(0);
  let leadFloorSources = $state<LeadFloorSource[]>([]);
  // Whether sendspin delay changes apply to the running stream (future firmware)
  // or need a stream restart (current ESPHome firmware).
  let sendspinDelayLive = $state(false);
  let delayModeBusy = $state(false);

  // Whether the HA integration also exposes a per-output media_player (on top of
  // the per-music-group and per-announcement-group entities it creates by default).
  let exposeOutputs = $state(false);
  let exposeBusy = $state(false);

  async function refresh() {
    loading = true;
    try {
      const [s, sync] = await Promise.all([api.settings(), api.syncSettings().catch(() => null)]);
      duck = s.default_duck;
      discovery = s.discovery_enabled;
      sendspinDelayLive = s.sendspin_delay_live;
      exposeOutputs = s.expose_outputs_as_media_players;
      if (sync) {
        groupLeadMs = sync.group_lead_ms;
        leadFloorMs = sync.group_lead_floor_ms;
        leadEffectiveMs = sync.group_lead_effective_ms;
        leadFloorSources = sync.group_lead_floor_sources;
      }
    } catch {
      // leave defaults; a toast already surfaced the error via run() elsewhere
    }
    loading = false;
  }
  onMount(refresh);

  async function saveGeneral() {
    generalBusy = true;
    await run(() => api.setSettings({ default_duck: duck }), 'Settings saved');
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
    // Surface the daemon's own message: it says when the stored value is below the
    // floor and which speaker raised it, rather than us implying the number took.
    try {
      const res = await api.setGroupLead(Number(groupLeadMs));
      toast(res.ok === false ? 'error' : 'success', res.message ?? `Group lead set to ${groupLeadMs} ms`);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
    await refresh(); // the effective value (and the floor) may have moved
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

  async function toggleExposeOutputs() {
    exposeBusy = true;
    const next = !exposeOutputs;
    if (await run(() => api.setSettings({ expose_outputs_as_media_players: next }), 'Setting saved')) {
      exposeOutputs = next;
    }
    exposeBusy = false;
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
        <!-- min is the device-reported floor, not 0: the daemon raises the send-ahead
             to it regardless, so offering lower values would only mislead. -->
        <input
          id="group-lead"
          type="number"
          min={leadFloorMs}
          max="5000"
          step="10"
          bind:value={groupLeadMs}
          placeholder="250"
          title={leadFloorMs > 0
            ? `At least ${leadFloorMs} ms — that is what your speakers ask for with their current codec`
            : 'How far ahead audio is scheduled'}
        />
      </div>
      <div class="field">
        <button class="primary" onclick={saveGroupLead} disabled={syncBusy || groupLeadMs === ''}>Apply</button>
      </div>
    </div>
    {#if leadFloorMs > 0}
      <p class="muted" style="font-size:0.8rem; margin:6px 0 0">
        At least <strong>{leadFloorMs} ms</strong> is needed here, so that is the lowest value with any effect — the
        add-on uses <strong>{leadEffectiveMs} ms</strong>.
        {#if leadFloorSources.length}
          <br />
          {leadFloorSources[0].name} needs {leadFloorSources[0].required_ms} ms:
          {#if leadFloorSources[0].min_buffer_ms != null}
            it asks for {leadFloorSources[0].min_buffer_ms} ms of buffer
          {:else if leadFloorSources[0].codec_minimum_ms > 0}
            {leadFloorSources[0].codec} needs {leadFloorSources[0].codec_minimum_ms} ms to decode in time
          {:else}
            no buffer requirement of its own
          {/if}
          {#if leadFloorSources[0].static_delay_ms > 0}
            + its {leadFloorSources[0].static_delay_ms} ms speaker delay (it plays that early, so audio must be sent
            that much sooner)
          {/if}.
        {/if}
        A speaker can need more with a compressed codec than with PCM, so this floor moves when you change a codec on
        the Outputs tab.
      </p>
    {/if}

    <label class="check" style="margin-top:12px">
      <input type="checkbox" checked={sendspinDelayLive} disabled={delayModeBusy} onchange={toggleDelayLive} />
      Sendspin speakers apply delay changes live
    </label>
    <p class="muted" style="font-size:0.8rem; margin:4px 0 0">
      Leave off for current ESPHome firmware, which only picks up a per-speaker delay at stream start — so changing one
      reconnects <em>that</em> speaker (the rest of the group keeps playing). Turn on only for firmware that applies
      <code>SetStaticDelay</code> to the running stream, to avoid the reconnect.
    </p>
  </div>

  <div class="card">
    <h2>Home Assistant entities</h2>
    <p class="card-sub">
      The Home Assistant integration creates one <code>media_player</code> per <strong>music group</strong> and per
      <strong>announcement group</strong>. Enable this to <strong>also</strong> expose every individual output as its own
      <code>media_player</code>, for directly addressing a single speaker regardless of its group.
    </p>
    <label class="check">
      <input type="checkbox" checked={exposeOutputs} disabled={exposeBusy} onchange={toggleExposeOutputs} />
      Expose all outputs as individual media players
    </label>
    <p class="muted" style="font-size:0.8rem; margin:4px 0 0">
      Off by default. Changes take effect after the integration next reconciles its entities (within a few seconds, or on
      the next Home Assistant restart).
    </p>
  </div>
{/if}
