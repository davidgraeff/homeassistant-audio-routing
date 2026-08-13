<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../../lib/api';
  import { run, toast } from '../../lib/toast';
  import DelaySlider from '../ui/DelaySlider.svelte';
  import type { LeadFloorSource } from '../../lib/types';

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
  // What the running groups are streaming at, which is NOT the same thing as the
  // effective value above: a running send-ahead only ratchets up on its own, so a group
  // can sit on an old, larger figure indefinitely. Shown whenever the two differ —
  // without it this page claimed 130 ms while 900 ms was playing (2026-08-12), which is
  // how tuning for low latency turned into guesswork.
  let leadRunningMs = $state<number | null>(null);
  // Whether sendspin delay changes apply to the running stream (future firmware)
  // or need a stream restart (current ESPHome firmware).
  let sendspinDelayLive = $state(false);
  let delayModeBusy = $state(false);

  // Whether the HA integration also exposes a per-output media_player (on top of
  // the per-music-group and per-announcement-group entities it creates by default).
  let exposeOutputs = $state(false);
  let exposeBusy = $state(false);

  /** The group lead the daemon has stored, as opposed to the field's pending edit —
   *  the Opus slider has to send it back unchanged. */
  let appliedGroupLeadMs = $state(0);

  async function refresh() {
    loading = true;
    try {
      const [s, sync] = await Promise.all([api.settings(), api.syncSettings().catch(() => null)]);
      duck = s.default_duck;
      discovery = s.discovery_enabled;
      sendspinDelayLive = s.sendspin_delay_live;
      exposeOutputs = s.expose_outputs_as_media_players;
      if (sync) {
        opusFloorApplied = sync.opus_floor_ms;
        opusFloorMinMs = sync.opus_floor_min_ms;
        appliedGroupLeadMs = sync.group_lead_ms;
        groupLeadMs = sync.group_lead_ms;
        leadFloorMs = sync.group_lead_floor_ms;
        leadEffectiveMs = sync.group_lead_effective_ms;
        leadRunningMs = sync.group_lead_running_ms ?? null;
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

  // The Opus decode/network headroom, on its own slider: it commits on release rather
  // than on the group lead's Apply button, because each change restarts the sendspin
  // group's stream and one commit per gesture is the right rate for that.
  let opusFloorApplied = $state(250);
  let opusFloorMinMs = $state(20);

  async function saveOpusFloor(ms: number) {
    // Sent alongside the *stored* group lead, since the endpoint takes both and the
    // lead field may be holding an edit the user hasn't applied yet.
    try {
      const res = await api.setGroupLead(appliedGroupLeadMs, ms);
      toast('success', res.message ?? `Opus headroom set to ${ms} ms`);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
    await refresh(); // the floor it produces, and which speaker sets it, may have moved
  }

  async function saveGroupLead() {
    if (groupLeadMs === '' || Number(groupLeadMs) < 0) return;
    syncBusy = true;
    // Surface the daemon's own message: it says when the stored value is below the
    // floor and which speaker raised it, rather than us implying the number took.
    try {
      const res = await api.setGroupLead(Number(groupLeadMs));
      toast('success', res.message ?? `Group lead set to ${groupLeadMs} ms`);
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
      Finds AirPlay and Sendspin devices on your network via mDNS and lists them on the Outputs page, where you add the
      ones you want — being discovered on its own does nothing. Turning this off stops
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
      <strong>group lead</strong> is how far ahead audio is scheduled — the buffer every speaker in the group plays out
      of, and the latency you pay for it. The default of <strong>180 ms</strong> is measured, not theoretical: it is the
      smallest lead ESP32 speakers played cleanly at over 2.4 GHz WiFi here, and it exists because that firmware never
      says how much buffer it needs. Your radio environment is not this one, so treat it as a starting point: lower it
      until you hear stutter, then go back one step. Fine-tune an individual speaker with its per-output value on the
      Outputs tab.
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
          placeholder="180"
          title={leadFloorMs > 0
            ? `At least ${leadFloorMs} ms — that is what your speakers ask for with their current codec`
            : 'How far ahead audio is scheduled'}
        />
      </div>
      <div class="field">
        <button class="primary" onclick={saveGroupLead} disabled={syncBusy || groupLeadMs === ''}>Apply</button>
      </div>
    </div>
    <!-- The same control as every other latency knob (DelaySlider): bounded by what
         the daemon accepts rather than by taste, and committing on release only — each
         commit restarts the sendspin group's stream, so one value per drag is exactly
         the right rate. -->
    <div class="row" style="margin-top:4px">
      <DelaySlider
        id="opus-floor"
        label="Opus headroom"
        applied={opusFloorApplied}
        min={opusFloorMinMs}
        max={300}
        step={10}
        riskyBelow={30}
        highAbove={80}
        risk="a block has under half its own length of slack to cross the network and be decoded — expect stutter"
        good="Head start for the WiFi hop and the speaker's Opus decode"
        origin={opusFloorApplied === 40 ? 'the shipped default' : 'your value'}
        deferredHint=" — on release"
        oncommit={saveOpusFloor}
      />
    </div>
    <p class="muted" style="font-size:0.8rem; margin:6px 0 0">
      <strong>Opus headroom</strong> is the part of the lead that is neither your choice nor a speaker's request: an Opus
      block has to arrive, be decoded on the speaker's MCU and be scheduled before its play time, so every Opus group
      gets at least this much however low the group lead is set. The floor is {opusFloorMinMs} ms — one block, since
      nothing is sent before a whole block exists — but low values are where ESP32 speakers stutter: a WiFi retransmit
      or a roam scan eats tens of milliseconds, and this is the only budget it has to come out of. If speakers stutter,
      raise this (or the group lead) until they stop, then come back down to the smallest value that still plays clean.
      PCM and FLAC ignore it, and a speaker that states its own buffer requirement overrides it.
    </p>
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
    <!-- The value in force, whenever it is not the value computed above. These diverge
         by design — a running send-ahead is fixed when the group's timeline is built and
         only ever ratchets *up* on its own — and not showing it is what made this page
         report 130 ms while 900 ms was playing. Applying now brings it down, at the price
         of every speaker in the group reconnecting. -->
    {#if leadRunningMs != null && leadRunningMs !== leadEffectiveMs}
      <p class="lead-running" style="font-size:0.8rem; margin:6px 0 0">
        Streaming right now at <strong>{leadRunningMs} ms</strong>, not {leadEffectiveMs} ms. A group's lead is fixed
        when its stream starts and only rises by itself — something raised it (a speaker delay, a codec change, or a
        higher setting) and it stayed. Press <strong>Apply</strong> to restart the group(s) at {leadEffectiveMs} ms; each
        speaker reconnects, which costs it a few seconds of silence.
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
      <code>media_player</code>, for directly addressing a single speaker regardless of its group. Only outputs you
      <strong>added</strong> on the Outputs page count — a merely discovered device never becomes an entity.
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

<style>
  /* A number that contradicts the one the page just showed. Amber like `.badge.caution`
     (app.css), for the same reason: a group streaming at a stale lead is not an error —
     it plays fine — but it is not what the setting says, and that is worth noticing. */
  .lead-running {
    color: var(--warning-color, #b26a00);
  }
</style>
