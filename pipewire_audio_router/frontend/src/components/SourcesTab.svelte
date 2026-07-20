<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';
  import type { AirplayClient, AirplaySourceInfo, RtpSourceInfo } from '../lib/types';

  let airplay = $state<AirplaySourceInfo>({
    name: null,
    running: false,
    latency_msec: 150,
    auth_setup: false,
    prevent_takeover: true,
  });
  let airplayName = $state('');
  let airplayLatency = $state(150);
  let airplayAuthSetup = $state(false);
  let airplayPreventTakeover = $state(true);
  let rtp = $state<RtpSourceInfo>({
    enabled: false,
    port: 46000,
    latency_msec: 200,
    source_addr: '0.0.0.0',
    ignore_ssrc: true,
    loaded: false,
  });
  let rtpPort = $state(46000);
  let rtpLatency = $state(200);
  // The "Source" radio: which senders this receiver accepts. Two of the three
  // modes pin source.ip to 0.0.0.0 and differ only in sess.ignore-ssrc; the
  // multicast mode reveals the group-address field. Kept in the UI (rather than
  // one raw source.ip field + a checkbox) to match how people actually reason
  // about it. See mode→(source_addr, ignore_ssrc) mapping in saveRtp/refresh.
  type RtpMode = 'all' | 'multicast' | 'single';
  let rtpMode = $state<RtpMode>('all');
  let rtpMulticastAddr = $state('239.255.42.42');
  let busy = $state(false);

  // A non-empty, non-0.0.0.0 source.ip means the receiver joined a multicast
  // group; otherwise ignore-ssrc distinguishes "accept all" from "single".
  function deriveRtpMode(addr: string, ignoreSsrc: boolean): RtpMode {
    if (addr && addr !== '0.0.0.0') return 'multicast';
    return ignoreSsrc ? 'all' : 'single';
  }

  // Remembered AirPlay senders (current + previous). Polled while this tab is
  // open so the connection badge and the manage-connections list stay live.
  let clients = $state<AirplayClient[]>([]);
  let showManage = $state(false);

  const connectedClients = $derived(clients.filter((c) => c.connected));

  function clientLabel(c: AirplayClient): string {
    return c.name ?? c.addr;
  }

  function formatWhen(unixSecs: number): string {
    if (!unixSecs) return 'never';
    return new Date(unixSecs * 1000).toLocaleString();
  }

  async function refreshClients() {
    try {
      clients = await api.airplayClients();
    } catch {
      /* keep last-known */
    }
  }

  async function refresh() {
    try {
      airplay = await api.airplaySource();
      airplayName = airplay.name ?? '';
      airplayLatency = airplay.latency_msec;
      airplayAuthSetup = airplay.auth_setup;
      airplayPreventTakeover = airplay.prevent_takeover;
    } catch {
      /* keep last-known */
    }
    try {
      rtp = await api.rtpSource();
      rtpPort = rtp.port;
      rtpLatency = rtp.latency_msec;
      rtpMode = deriveRtpMode(rtp.source_addr, rtp.ignore_ssrc);
      // Keep the group field populated with the stored group (so it round-trips)
      // but never with 0.0.0.0 — the field only shows in multicast mode.
      if (rtpMode === 'multicast') rtpMulticastAddr = rtp.source_addr;
    } catch {
      /* keep last-known */
    }
    await refreshClients();
  }

  onMount(() => {
    refresh();
    // Light poll so connect/disconnect shows up without a manual reload.
    const timer = setInterval(refreshClients, 5000);
    return () => clearInterval(timer);
  });

  async function forgetClient(c: AirplayClient) {
    if (await run(() => api.forgetAirplayClient(c.key), `Forgot '${clientLabel(c)}'`)) {
      await refreshClients();
    }
  }

  async function banClient(c: AirplayClient, banned: boolean) {
    const msg = banned ? `Banned '${clientLabel(c)}'` : `Unbanned '${clientLabel(c)}'`;
    if (await run(() => api.banAirplayClient(c.key, banned), msg)) {
      await refreshClients();
    }
  }

  async function setPriority(c: AirplayClient, priority: number) {
    if (await run(() => api.setAirplayClientPriority(c.key, priority), `Priority ${priority} for '${clientLabel(c)}'`)) {
      await refreshClients();
    }
  }

  async function disconnectClient(c: AirplayClient) {
    if (await run(() => api.disconnectAirplayClient(c.key), `Disconnecting '${clientLabel(c)}'`)) {
      await refreshClients();
    }
  }

  // Live toggle — no receiver restart, so the current stream isn't disturbed.
  async function savePreventTakeover() {
    await run(
      () => api.setAirplayPolicy(airplayPreventTakeover),
      airplayPreventTakeover ? 'New senders refused while one is streaming' : 'New senders may take over the stream',
    );
    await refresh();
  }

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

  // Map the "Source" radio to the two backend knobs. Multicast joins a group
  // (any sender on it); "single" binds all interfaces but locks onto the first
  // SSRC and rejects the rest; "all" is the plain accept-everything listener.
  function rtpModeToParams(): { sourceAddr: string; ignoreSsrc: boolean } {
    if (rtpMode === 'multicast') return { sourceAddr: rtpMulticastAddr.trim() || '239.255.42.42', ignoreSsrc: true };
    if (rtpMode === 'single') return { sourceAddr: '0.0.0.0', ignoreSsrc: false };
    return { sourceAddr: '0.0.0.0', ignoreSsrc: true };
  }

  async function saveRtp(e: Event) {
    e.preventDefault();
    busy = true;
    const { sourceAddr, ignoreSsrc } = rtpModeToParams();
    const where = rtpMode === 'multicast' ? sourceAddr : rtpMode === 'single' ? 'single sender' : 'any sender';
    const ok = await run(
      () => api.setRtpSource(rtpPort, rtpLatency, sourceAddr, ignoreSsrc),
      `RTP source enabled on :${rtpPort} (${where}, ${rtpLatency} ms buffer)`,
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
      <div class="field">
        <label for="ap-takeover" title="Refuse a new sender while one is already streaming (applied live, no restart)">
          Prevent takeover
        </label>
        <input id="ap-takeover" type="checkbox" bind:checked={airplayPreventTakeover} onchange={savePreventTakeover} />
      </div>
      <div class="field"><button class="primary" type="submit" disabled={busy}>Save</button></div>
      <div class="field">
        <button class="ghost" type="button" onclick={disableAirplay} disabled={busy || !airplay.name}>Disable</button>
      </div>
    </div>
  </form>
  <div class="row" style="align-items:center; gap:0.75rem">
    <p class="muted" style="font-size:0.85rem; margin:0; flex:1 1 auto">
      Status:
      {#if airplay.name}
        <span class="badge {airplay.running ? 'on' : 'off'}">{airplay.running ? 'running' : 'stopped'}</span>
        as <strong>{airplay.name}</strong>
        {#if connectedClients.length}
          — <span class="badge on">streaming</span>
          from
          <strong>{connectedClients.map(clientLabel).join(', ')}</strong>
        {:else}
          — <span class="badge off">no client</span>
        {/if}
      {:else}
        <span class="badge off">disabled</span>
      {/if}
    </p>
    <button class="ghost" type="button" onclick={() => { showManage = true; refreshClients(); }}>
      Manage connections{#if clients.length}&nbsp;({clients.length}){/if}
    </button>
  </div>
</div>

{#if showManage}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="modal-backdrop" onclick={() => (showManage = false)}>
    <div class="modal-card card" onclick={(e) => e.stopPropagation()}>
      <div class="row" style="align-items:center; justify-content:space-between">
        <h2 style="margin:0">AirPlay connections</h2>
        <button class="ghost" type="button" onclick={() => (showManage = false)}>Close</button>
      </div>
      <p class="card-sub">
        Senders that have connected to this AirPlay source. Currently-connected devices are shown live.
        <strong>Ban</strong> refuses a client's future connections (matched by name if known, else by IP;
        it doesn't drop a stream already playing — use <strong>Disconnect</strong> for that).
        <strong>Priority</strong> arbitrates takeover: a connecting sender with a strictly higher priority than
        the current one takes the stream over; otherwise the "Prevent takeover" policy applies.
        Forgetting is for previously-seen clients.
      </p>
      {#if clients.length === 0}
        <p class="empty">No AirPlay client has connected yet.</p>
      {:else}
        <table class="clients">
          <thead>
            <tr><th>Client</th><th>Address</th><th>Last connected</th><th>Priority</th><th></th></tr>
          </thead>
          <tbody>
            {#each clients as c (c.key)}
              <tr>
                <td>
                  <strong>{clientLabel(c)}</strong>
                  {#if c.connected}<span class="badge on">connected</span>{/if}
                  {#if c.banned}<span class="badge off" title="Future connections refused">banned</span>{/if}
                  {#if !c.name}<span class="badge off" title="Sender advertised no name">IP&nbsp;only</span>{/if}
                </td>
                <td class="muted">{c.addr}</td>
                <td class="muted">{formatWhen(c.last_connected)}</td>
                <td>
                  <input
                    class="prio"
                    type="number"
                    step="1"
                    value={c.priority}
                    title="Higher priority takes over a lower-priority stream"
                    onchange={(e) => setPriority(c, Number((e.currentTarget as HTMLInputElement).value) || 0)}
                  />
                </td>
                <td style="text-align:right; white-space:nowrap">
                  {#if c.connected}
                    <button class="ghost danger" type="button" title="Drop this client's current connection" onclick={() => disconnectClient(c)}>
                      Disconnect
                    </button>
                  {/if}
                  {#if c.banned}
                    <button class="ghost" type="button" title="Allow this client again" onclick={() => banClient(c, false)}>
                      Unban
                    </button>
                  {:else}
                    <button class="ghost danger" type="button" title="Refuse this client's future connections" onclick={() => banClient(c, true)}>
                      Ban
                    </button>
                  {/if}
                  <button
                    class="ghost danger"
                    type="button"
                    disabled={c.connected}
                    title={c.connected ? 'Disconnect before forgetting' : 'Forget this client'}
                    onclick={() => forgetClient(c)}
                  >
                    Forget
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
    </div>
  </div>
{/if}

<div class="card">
  <h2>Bluetooth bridge (RTP) source</h2>
  <p class="card-sub">
    Receives the RTP audio stream from the ESP32 Bluetooth bridge firmware and exposes it as a routable
    source. The listen port must match the port the firmware sends to. Raise the jitter buffer if a
    weak-signal bridge stutters — it trades latency for dropout tolerance.
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
      <div class="field"><button class="primary" type="submit" disabled={busy}>Enable</button></div>
      <div class="field">
        <button class="ghost" type="button" onclick={disableRtp} disabled={busy || !rtp.enabled}>Disable</button>
      </div>
    </div>
    <fieldset class="rtp-source">
      <legend>Source</legend>
      <label class="radio">
        <input type="radio" name="rtp-mode" value="all" bind:group={rtpMode} />
        <span>
          <strong>Accept all senders</strong>
          <span class="muted">Any device sending to this port is received. Packets from two senders may interleave and corrupt the audio.</span>
        </span>
      </label>
      <label class="radio">
        <input type="radio" name="rtp-mode" value="single" bind:group={rtpMode} />
        <span>
          <strong>Only one client</strong>
          <span class="muted">Locks onto the first sender's stream and rejects all others — the corruption guard. Needs firmware with a stable SSRC (any recent bt-bridge build).</span>
        </span>
      </label>
      <label class="radio">
        <input type="radio" name="rtp-mode" value="multicast" bind:group={rtpMode} />
        <span>
          <strong>Multicast group</strong>
          <span class="muted">Join a group so several boxes share one stream. Set the same group on every receiver and point the firmware's RTP host at it. IPv4 or IPv6.</span>
        </span>
      </label>
      {#if rtpMode === 'multicast'}
        <div class="field multicast-addr">
          <label for="rtp-mcast">Group address</label>
          <input id="rtp-mcast" type="text" bind:value={rtpMulticastAddr} placeholder="239.255.42.42" />
        </div>
      {/if}
    </fieldset>
  </form>
  <p class="muted" style="font-size:0.85rem; margin:0">
    Status:
    {#if rtp.enabled}
      <span class="badge {rtp.loaded ? 'on' : 'off'}">{rtp.loaded ? 'loaded' : 'not loaded'}</span>
      on <strong>:{rtp.port}</strong>, <strong>{rtp.latency_msec} ms</strong> buffer —
      {#if rtp.source_addr !== '0.0.0.0'}
        multicast <strong>{rtp.source_addr}</strong>
      {:else if rtp.ignore_ssrc}
        <strong>any sender</strong>
      {:else}
        <strong>single sender</strong>
      {/if}
    {:else}
      <span class="badge off">disabled</span>
    {/if}
  </p>
</div>

<style>
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
    width: min(720px, 100%);
    max-height: 84vh;
    overflow: auto;
    margin: 0;
  }
  table.clients {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.9rem;
  }
  table.clients th,
  table.clients td {
    text-align: left;
    padding: 0.4rem 0.5rem;
    border-bottom: 1px solid var(--divider-color);
    vertical-align: middle;
  }
  table.clients th {
    font-weight: 600;
    color: var(--secondary-text-color);
  }
  table.clients .badge {
    margin-left: 0.4rem;
  }
  input.prio {
    width: 4rem;
  }
  fieldset.rtp-source {
    margin: 0.75rem 0 0;
    padding: 0.5rem 0.75rem 0.75rem;
    border: 1px solid var(--divider-color);
    border-radius: 6px;
  }
  fieldset.rtp-source legend {
    padding: 0 0.35rem;
    font-weight: 600;
    color: var(--secondary-text-color);
    font-size: 0.85rem;
  }
  fieldset.rtp-source label.radio {
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
    padding: 0.3rem 0;
    cursor: pointer;
  }
  fieldset.rtp-source label.radio input {
    margin-top: 0.2rem;
  }
  fieldset.rtp-source label.radio span {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
  }
  fieldset.rtp-source .muted {
    font-size: 0.82rem;
  }
  fieldset.rtp-source .multicast-addr {
    margin: 0.4rem 0 0 1.6rem;
    max-width: 22rem;
  }
</style>
