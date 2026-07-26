<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run, toast } from '../lib/toast';
  import type { OutputInfo } from '../lib/types';
  import VolumeControl from './VolumeControl.svelte';

  let outputs = $state<OutputInfo[]>([]);
  let loading = $state(true);

  // Per-device volume (integer 0–100 percent, or null when UNKNOWN) and mute for
  // present sendspin / AirPlay-2 outputs, keyed by node name. Seeded on refresh
  // from OutputInfo.ap2_volume/ap2_muted (AP2, 0–1) or api.sendspinVolumes()
  // (sendspin, already 0–100); updated optimistically on user input. This tab
  // has no live WebSocket, so there's no incoming stream to fight the thumb.
  let vol = $state<Record<string, number | null>>({});
  let muted = $state<Record<string, boolean>>({});

  // Per-device sync tuning: static delays (sendspin) and per-output render delay
  // (AirPlay 2). The daemon-wide group lead lives on the Settings tab.
  // Per-row editable sync value (ms): AP2 render delay or sendspin static delay.
  let edit = $state<Record<string, number | ''>>({});

  async function refresh() {
    loading = true;
    try {
      const [outs, delays, sVols] = await Promise.all([
        api.outputs(),
        api.sendspinDelays().catch(() => ({}) as Record<string, number>),
        api.sendspinVolumes().catch(() => ({}) as Record<string, number>),
      ]);
      outputs = outs;
      // Seed the editable sync fields + volume/mute state from current state.
      const next: Record<string, number | ''> = {};
      const vNext: Record<string, number | null> = {};
      const mNext: Record<string, boolean> = {};
      for (const o of outs) {
        if (o.kind === 'sendspin') {
          next[o.node_name] = delays[o.node_name] ?? 0;
          // undefined (not reported) → null = UNKNOWN (slider shows 0), never 100.
          const sv = sVols[o.node_name];
          vNext[o.node_name] = sv == null ? null : Math.round(sv);
          // No sendspin mute-state endpoint; preserve any optimistic value.
          mNext[o.node_name] = muted[o.node_name] ?? false;
        } else {
          next[o.node_name] = o.latency_ms ?? '';
          if (o.kind === 'airplay2') {
            vNext[o.node_name] = o.ap2_volume == null ? null : Math.round(o.ap2_volume * 100);
            mNext[o.node_name] = o.ap2_muted ?? false;
          }
        }
      }
      edit = next;
      vol = vNext;
      muted = mNext;
    } catch {
      outputs = [];
    }
    loading = false;
  }
  onMount(refresh);

  // Per-row diagnostic playback ("Play tone" / "Play announcement") via the
  // per-device announce path (`/api/announce`) — the backend-agnostic route
  // that ducks + overlays a clip on one device. Keyed by node name so only the
  // pressed row's buttons show the busy state.
  let testing = $state<Record<string, 'tone' | 'announce' | null>>({});

  // Per-output collapse state, keyed by node name. Cards start collapsed so the
  // full list of outputs is visible at a glance without scrolling; expanding a
  // card reveals its connection details, sync tuning, and remove/configure.
  let collapsed = $state<Record<string, boolean>>({});
  function toggle(o: OutputInfo) {
    collapsed = { ...collapsed, [o.node_name]: !isCollapsed(o) };
  }
  function isCollapsed(o: OutputInfo): boolean {
    // Default to collapsed when we haven't tracked this output yet.
    return collapsed[o.node_name] ?? true;
  }

  function kindLabel(o: OutputInfo): string {
    return o.kind === 'sendspin' ? 'Sendspin' : 'AirPlay 2';
  }

  // PTP-lock badge for an AirPlay-2 output — tri-state, because a lock is not
  // needed for single-room realtime playback (the receiver free-runs off our
  // PT=87 anchors); it only prevents drift in a multi-room group. So we only
  // *alarm* (red) when the receiver is both unlocked AND in a ≥2-member group.
  // Returns null when no badge should show (non-AP2 or offline).
  function ptpBadge(o: OutputInfo): { cls: string; text: string; title: string } | null {
    if (o.kind !== 'airplay2' || !o.present) return null;
    const age = o.ptp_lock_age_s != null ? ` (last clock sync ${o.ptp_lock_age_s}s ago)` : '';
    if (o.ptp_locked) {
      return { cls: 'badge on', text: 'PTP ✓', title: `Exchanging PTP with our clock${age} — multi-room sync is tight.` };
    }
    if (o.ptp_supported === false) {
      return {
        cls: 'badge',
        text: 'PTP n/a',
        title: "This receiver doesn't advertise PTP support (features bit 41); our sender streams realtime without it.",
      };
    }
    if (o.ptp_relevant) {
      return {
        cls: 'badge warn',
        text: 'no PTP lock',
        title: `Not exchanging PTP with us${age}. This receiver is in a multi-room AirPlay-2 group, so without a shared clock the rooms can drift out of sync. Re-route it (disconnect then reconnect) to re-establish PTP.`,
      };
    }
    return {
      cls: 'badge',
      text: 'PTP —',
      title: `Not exchanging PTP with us${age} — fine for single-room realtime playback; a live PTP lock only matters for keeping multiple rooms in sync.`,
    };
  }

  // Which outputs can be announced to individually via the per-device path
  // (/api/announce). Both output kinds are per-device senders: sendspin is wired
  // into OverlayMixer, and AirPlay 2 goes through its own announce/overlay path.
  function canTest(o: OutputInfo): boolean {
    return o.present && (o.kind === 'sendspin' || o.kind === 'airplay2');
  }
  function testHint(o: OutputInfo): string {
    if (!o.present) return 'Output is offline';
    return '';
  }

  async function playTone(o: OutputInfo) {
    testing = { ...testing, [o.node_name]: 'tone' };
    await run(() => api.announce({ targets: [o.node_name], tone: true }), `Played test tone on '${o.name}'`);
    testing = { ...testing, [o.node_name]: null };
  }
  async function playAnnouncement(o: OutputInfo) {
    testing = { ...testing, [o.node_name]: 'announce' };
    await run(() => api.announce({ targets: [o.node_name], test: true }), `Played test announcement on '${o.name}'`);
    testing = { ...testing, [o.node_name]: null };
  }

  // Per-device volume / mute for present sendspin + AirPlay-2 outputs, from the
  // always-visible card header. Optimistic (no live stream on this tab).
  async function onVolume(o: OutputInfo, pct: number) {
    vol = { ...vol, [o.node_name]: pct };
    try {
      if (o.kind === 'airplay2') await api.setAp2Volume(o.node_name, pct / 100);
      else await api.setSendspinVolume(o.node_name, pct);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
  async function onMute(o: OutputInfo) {
    const next = !muted[o.node_name];
    muted = { ...muted, [o.node_name]: next };
    try {
      if (o.kind === 'airplay2') await api.setAp2Mute(o.node_name, next);
      else await api.setSendspinMute(o.node_name, next);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }

  // AirPlay-2 wire sample-rate mode (auto vs forced 44.1 kHz). Restarts the group.
  async function setRateMode(o: OutputInfo, e: Event) {
    const mode = (e.currentTarget as HTMLSelectElement).value as 'auto' | 'fixed_44100';
    await run(() => api.setAp2RateMode(o.node_name, mode),
      `Set '${o.name}' sample rate to ${mode === 'auto' ? 'auto' : '44.1 kHz'}`);
    await refresh();
  }

  // Apply the per-row sync value: sendspin → static delay; AP2 → render delay
  // (via setOutputLatency).
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
</script>

<div class="card info">
  <h2>Supported outputs</h2>
  <p class="card-sub">
    This router can stream to <strong>AirPlay 2</strong> receivers (AV receivers, HomePods, AirPlay
    speakers) and <strong>Sendspin</strong> speakers — the open multi-room protocol used by ESPHome and
    Home Assistant Voice PE. Compatible devices on your network are discovered automatically and appear
    below and in the routing matrix; you don't need to configure anything. Route one source to several
    Sendspin devices — or a mix of Sendspin and AirPlay 2 — and they play in one synchronized group.
  </p>
  <p class="card-sub" style="margin-bottom:0">
    Each output below is everything this router can send audio to right now. Auto-discovered devices are
    tagged <span class="badge auto">auto</span>; <span class="badge off">offline</span> ones have saved
    routing but aren't currently on the network.
  </p>
</div>

{#if loading}
  <div class="card"><p class="empty" style="padding:0">Loading…</p></div>
{:else if outputs.length === 0}
  <div class="card">
    <p class="empty" style="padding:0">
      No outputs yet. Compatible AirPlay 2 / Sendspin devices appear here automatically when they're on the
      network.
    </p>
  </div>
{:else}
  {#each outputs as o (o.node_name)}
    <article class="card out-card" class:offline={!o.present} class:collapsed={isCollapsed(o)}>
      <header class="out-head">
        <button
          class="collapse-toggle"
          aria-expanded={!isCollapsed(o)}
          title={isCollapsed(o) ? 'Show details' : 'Hide details'}
          onclick={() => toggle(o)}
        >
          <span class="chevron">▶</span>
        </button>
        <div class="out-title">
          <h3>{o.name}</h3>
          <div class="out-badges">
            <span class="badge">{kindLabel(o)}</span>
            {#if !o.configured}<span class="badge auto" title="Found via mDNS auto-discovery">auto</span>{/if}
            <span class="badge {o.present ? 'on' : 'off'}">{o.present ? 'online' : 'offline'}</span>
            {#if ptpBadge(o)}
              {@const ptp = ptpBadge(o)!}
              <span class={ptp.cls} title={ptp.title}>{ptp.text}</span>
            {/if}
          </div>
        </div>
        {#if o.present && (o.kind === 'airplay2' || o.kind === 'sendspin')}
          <div class="out-vol">
            <VolumeControl
              percent={vol[o.node_name]}
              muted={muted[o.node_name] ?? false}
              onVolume={(pct) => onVolume(o, pct)}
              onMute={() => onMute(o)}
            />
          </div>
        {/if}
        <div class="btn-group out-play" title={testHint(o)}>
          <button class="ghost seg label" disabled>Play:</button>
          <button class="ghost seg" disabled={!canTest(o) || testing[o.node_name] != null} onclick={() => playTone(o)}>
            {testing[o.node_name] === 'tone' ? 'Playing…' : 'Tone'}
          </button>
          <button class="ghost seg" disabled={!canTest(o) || testing[o.node_name] != null} onclick={() => playAnnouncement(o)}>
            {testing[o.node_name] === 'announce' ? 'Playing…' : 'Announcement'}
          </button>
        </div>
      </header>

      {#if !isCollapsed(o)}
        <dl class="out-meta">
          <div><dt>IP</dt><dd>{#if o.ip}<code>{o.ip}</code>{:else}—{/if}</dd></div>
          <div><dt>Port</dt><dd>{o.port ?? '—'}</dd></div>
          <div><dt>Encryption</dt><dd>{o.encryption ?? '—'}</dd></div>
        </dl>

        <div class="out-controls">
          <div class="sync-field">
            <label for="sync-{o.node_name}">
              {o.kind === 'sendspin' ? 'Static delay (ms)' : 'Render delay (ms)'}
            </label>
            <div class="sync-cell">
              <input
                id="sync-{o.node_name}"
                type="number"
                min={o.kind === 'sendspin' ? 0 : 200}
                max={o.kind === 'sendspin' ? 5000 : 2000}
                step="10"
                bind:value={edit[o.node_name]}
                placeholder={o.kind === 'sendspin' ? '0' : '1500'}
                title={o.kind === 'sendspin'
                  ? 'Static delay in ms (0 = none)'
                  : 'AirPlay 2 render delay in ms — how far ahead the receiver buffers before playing (blank = default 1500; clamped to 200–2000)'}
              />
              <button onclick={() => applySync(o)} title="Apply">Set</button>
            </div>
          </div>

          {#if o.kind === 'airplay2'}
            <div class="sync-field">
              <label for="rate-{o.node_name}">Sample rate</label>
              <div class="sync-cell">
                <select
                  id="rate-{o.node_name}"
                  value={o.ap2_rate_mode ?? 'auto'}
                  onchange={(e) => setRateMode(o, e)}
                  title="Auto negotiates 48 kHz and falls back to 44.1 kHz; fix 44.1 kHz for receivers that misbehave at 48 kHz"
                >
                  <option value="auto">Auto (negotiate 48 kHz)</option>
                  <option value="fixed_44100">AirPlay default (44.1 kHz)</option>
                </select>
                <span class="muted" style="white-space:nowrap">
                  {o.ap2_rate ? `${(o.ap2_rate / 1000).toFixed(1)} kHz` : ''}
                </span>
              </div>
            </div>
          {/if}
        </div>
      {/if}
    </article>
  {/each}
{/if}

<style>
  .badge.auto {
    background: color-mix(in srgb, var(--primary-color) 18%, transparent);
    color: var(--primary-color);
    border-color: transparent;
  }

  /* ---- One card per output ---------------------------------------------- */
  .out-card.offline {
    opacity: 0.6;
  }
  /* Collapsed cards drop their body; tighten the padding so the header row
     reads as a compact list item. */
  .out-card.collapsed {
    padding-top: 12px;
    padding-bottom: 12px;
  }
  .out-head {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .collapse-toggle {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    padding: 0;
    background: transparent;
    border: none;
    color: var(--secondary-text-color);
    cursor: pointer;
    border-radius: 6px;
  }
  .collapse-toggle:hover {
    background: color-mix(in srgb, var(--primary-color) 12%, transparent);
    color: var(--primary-color);
  }
  .chevron {
    font-size: 0.7rem;
    line-height: 1;
    transition: transform 0.15s ease;
  }
  .collapse-toggle[aria-expanded='true'] .chevron {
    transform: rotate(90deg);
  }
  .out-title {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    min-width: 0;
    flex: 1 1 auto;
  }
  .out-play {
    flex: 0 0 auto;
    margin-left: auto;
  }
  .out-vol {
    flex: 0 0 auto;
    width: 160px;
    max-width: 40%;
  }

  /* Connected, segmented play actions: a non-interactive "Play:" label segment
     followed by Tone / Announcement, sharing the ghost button look. */
  .btn-group {
    display: inline-flex;
    gap: 0;
  }
  .btn-group .seg {
    border-radius: 0;
    border: 1px solid var(--divider-color);
    border-left-width: 0;
  }
  .btn-group .seg:first-child {
    border-top-left-radius: 8px;
    border-bottom-left-radius: 8px;
    border-left-width: 1px;
  }
  .btn-group .seg:last-child {
    border-top-right-radius: 8px;
    border-bottom-right-radius: 8px;
  }
  .btn-group .seg.label {
    background: transparent;
    color: var(--secondary-text-color);
    font-weight: 500;
    cursor: default;
    opacity: 1;
  }
  .out-title h3 {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 500;
  }
  .out-badges {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }

  /* Connection details */
  .out-meta {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 12px;
    margin: 14px 0;
  }
  .out-meta dt {
    color: var(--secondary-text-color);
    font-size: 0.75rem;
  }
  .out-meta dd {
    margin: 2px 0 0;
    font-size: 0.95rem;
  }

  /* Sync + test controls */
  .out-controls {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
    padding-top: 14px;
    border-top: 1px solid var(--divider-color);
  }
  .sync-field label {
    display: block;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    margin-bottom: 4px;
  }
  .sync-cell {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .sync-cell input {
    width: 96px;
  }
</style>
