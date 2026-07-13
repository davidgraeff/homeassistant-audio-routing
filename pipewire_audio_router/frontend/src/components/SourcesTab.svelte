<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { AirplaySourceInfo, RtpSourceInfo } from '../lib/types';

  let airplay = $state<AirplaySourceInfo>({ name: null, running: false });
  let airplayName = $state('');
  let rtp = $state<RtpSourceInfo>({ enabled: false, port: 46000, loaded: false });
  let rtpPort = $state(46000);
  let busy = $state(false);

  async function refresh() {
    try {
      airplay = await api.airplaySource();
      airplayName = airplay.name ?? '';
    } catch {
      /* keep last-known */
    }
    try {
      rtp = await api.rtpSource();
      rtpPort = rtp.port;
    } catch {
      /* keep last-known */
    }
  }
  onMount(refresh);

  async function saveAirplay(e: Event) {
    e.preventDefault();
    busy = true;
    const trimmed = airplayName.trim();
    const ok = await run(
      () => api.setAirplaySource(trimmed),
      trimmed ? `AirPlay source set to '${trimmed}'` : 'AirPlay source disabled',
    );
    busy = false;
    if (ok) await refresh();
  }

  async function disableAirplay() {
    if (await run(() => api.disableAirplaySource(), 'AirPlay source disabled')) {
      airplayName = '';
      await refresh();
    }
  }

  async function saveRtp(e: Event) {
    e.preventDefault();
    busy = true;
    const ok = await run(() => api.setRtpSource(rtpPort), `RTP source enabled on port ${rtpPort}`);
    busy = false;
    if (ok) await refresh();
  }

  async function disableRtp() {
    if (await run(() => api.disableRtpSource(), 'RTP source disabled')) await refresh();
  }
</script>

<div class="card">
  <h2>AirPlay-receive source</h2>
  <p class="card-sub">
    A single AirPlay target that phones/PCs can cast into. Clear the name to disable it.
  </p>
  <form onsubmit={saveAirplay}>
    <div class="row">
      <div class="grow field">
        <label for="ap-name">Service name</label>
        <input id="ap-name" type="text" bind:value={airplayName} placeholder="PipeWire Router" />
      </div>
      <div class="field"><button class="primary" type="submit" disabled={busy}>Save</button></div>
      <div class="field">
        <button class="ghost" type="button" onclick={disableAirplay} disabled={busy || !airplay.name}>Disable</button>
      </div>
    </div>
  </form>
  <p class="muted" style="font-size:0.85rem; margin:0">
    Status:
    {#if airplay.name}
      <span class="badge {airplay.running ? 'on' : 'off'}">{airplay.running ? 'running' : 'stopped'}</span>
      as <strong>{airplay.name}</strong>
    {:else}
      <span class="badge off">disabled</span>
    {/if}
  </p>
</div>

<div class="card">
  <h2>Bluetooth bridge (RTP) source</h2>
  <p class="card-sub">
    Receives the RTP audio stream from the ESP32 Bluetooth bridge firmware and exposes it as a routable
    source. The listen port must match the port the firmware sends to.
  </p>
  <form onsubmit={saveRtp}>
    <div class="row">
      <div class="field">
        <label for="rtp-port">Listen port</label>
        <input id="rtp-port" type="number" min="1" max="65535" bind:value={rtpPort} placeholder="46000" />
      </div>
      <div class="field"><button class="primary" type="submit" disabled={busy}>Enable</button></div>
      <div class="field">
        <button class="ghost" type="button" onclick={disableRtp} disabled={busy || !rtp.enabled}>Disable</button>
      </div>
    </div>
  </form>
  <p class="muted" style="font-size:0.85rem; margin:0">
    Status:
    {#if rtp.enabled}
      <span class="badge {rtp.loaded ? 'on' : 'off'}">{rtp.loaded ? 'loaded' : 'not loaded'}</span>
      on port <strong>{rtp.port}</strong>
    {:else}
      <span class="badge off">disabled</span>
    {/if}
  </p>
</div>
