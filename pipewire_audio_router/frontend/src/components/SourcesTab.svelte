<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { AirplaySourceInfo, RtpSourceInfo } from '../lib/types';

  let airplay = $state<AirplaySourceInfo>({ name: null, running: false, latency_msec: 150, auth_setup: false });
  let airplayName = $state('');
  let airplayLatency = $state(150);
  let airplayAuthSetup = $state(false);
  let rtp = $state<RtpSourceInfo>({ enabled: false, port: 46000, latency_msec: 200, source_addr: '0.0.0.0', loaded: false });
  let rtpPort = $state(46000);
  let rtpLatency = $state(200);
  let rtpSourceAddr = $state('0.0.0.0');
  let busy = $state(false);

  async function refresh() {
    try {
      airplay = await api.airplaySource();
      airplayName = airplay.name ?? '';
      airplayLatency = airplay.latency_msec;
      airplayAuthSetup = airplay.auth_setup;
    } catch {
      /* keep last-known */
    }
    try {
      rtp = await api.rtpSource();
      rtpPort = rtp.port;
      rtpLatency = rtp.latency_msec;
      rtpSourceAddr = rtp.source_addr;
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
      () => api.setAirplaySource(trimmed, airplayLatency, airplayAuthSetup),
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
    const ok = await run(
      () => api.setRtpSource(rtpPort, rtpLatency, rtpSourceAddr.trim() || '0.0.0.0'),
      `RTP source enabled on ${rtpSourceAddr.trim() || '0.0.0.0'}:${rtpPort} (${rtpLatency} ms buffer)`,
    );
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
    A single AirPlay target that phones/PCs can cast into. Clear the name to disable it. Raise the
    jitter buffer if playback stutters — it trades latency for smoother audio. Enable "auth-setup" only
    if a sender refuses to connect unencrypted (it broadens compatibility but changes how PipeWire
    senders negotiate).
  </p>
  <form onsubmit={saveAirplay}>
    <div class="row">
      <div class="grow field">
        <label for="ap-name">Service name</label>
        <input id="ap-name" type="text" bind:value={airplayName} placeholder="PipeWire Router" />
      </div>
      <div class="field">
        <label for="ap-latency">Jitter buffer (ms)</label>
        <input id="ap-latency" type="number" min="20" max="2000" step="10" bind:value={airplayLatency} placeholder="150" />
      </div>
      <div class="field">
        <label for="ap-auth" title="Advertise the auth-setup encryption mode (et=0,4)">Auth-setup</label>
        <input id="ap-auth" type="checkbox" bind:checked={airplayAuthSetup} />
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
    source. The listen port must match the port the firmware sends to. Raise the jitter buffer if a
    weak-signal bridge stutters — it trades latency for dropout tolerance. Leave the source address as
    <code>0.0.0.0</code> for a normal (unicast) stream; set it to a multicast group (e.g.
    <code>239.255.42.42</code>) on every receiver — and point the firmware's RTP host at the same group —
    to share one stream across several boxes.
  </p>
  <form onsubmit={saveRtp}>
    <div class="row">
      <div class="field">
        <label for="rtp-port">Listen port</label>
        <input id="rtp-port" type="number" min="1" max="65535" bind:value={rtpPort} placeholder="46000" />
      </div>
      <div class="field">
        <label for="rtp-latency">Jitter buffer (ms)</label>
        <input id="rtp-latency" type="number" min="20" max="2000" step="10" bind:value={rtpLatency} placeholder="200" />
      </div>
      <div class="field">
        <label for="rtp-addr">Source address</label>
        <input id="rtp-addr" type="text" bind:value={rtpSourceAddr} placeholder="0.0.0.0" />
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
      on <strong>{rtp.source_addr}:{rtp.port}</strong>, <strong>{rtp.latency_msec} ms</strong> buffer
    {:else}
      <span class="badge off">disabled</span>
    {/if}
  </p>
</div>
