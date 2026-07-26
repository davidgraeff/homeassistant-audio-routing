<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { routing } from '../lib/routing';
  import { run } from '../lib/toast';
  import type { AirplayClient, AirplaySourceInfo, RtpSourceInfo } from '../lib/types';

  // Live input-level meters. The daemon meters present sources on-demand while
  // the routing matrix is watched — subscribing to the `routing` store here
  // opens that same WebSocket, so each snapshot carries these sources' current
  // peak (0.0–1.0). Match by the daemon's stable source node names (see
  // airplay_source.rs / rtp_source.rs). Same data the routing graph meter uses.
  const AIRPLAY_NODE = 'airplay-in';
  const RTP_NODE = 'bt-bridge-rtp';
  const sources = $derived($routing.matrix.sources);
  const airplayPeak = $derived(sources.find((s) => s.node_name === AIRPLAY_NODE)?.peak ?? 0);
  const rtpPeak = $derived(sources.find((s) => s.node_name === RTP_NODE)?.peak ?? 0);
  const pct = (peak: number) => Math.min(100, Math.round(peak * 100));

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

  // The three source modes, with the copy that used to live inline in the radio
  // list. Now driven from data so the compact dropdown can render the name in
  // the closed trigger and name+description in the open list, and the card can
  // show the selected mode's description on its own line below the control.
  const RTP_MODES: { value: RtpMode; label: string; desc: string }[] = [
    {
      value: 'all',
      label: 'Accept all senders',
      desc: 'Any device sending to this port is received. Packets from two senders may interleave and corrupt the audio.',
    },
    {
      value: 'single',
      label: 'Only one client',
      desc: "Locks onto the first sender's stream and rejects all others — the corruption guard. Needs firmware with a stable SSRC (any recent bt-bridge build).",
    },
    {
      value: 'multicast',
      label: 'Multicast group',
      desc: 'Join a group so several boxes share one stream. Set the same group on every receiver and point the firmware’s RTP host at it. IPv4 or IPv6.',
    },
  ];
  const rtpModeInfo = $derived(RTP_MODES.find((m) => m.value === rtpMode) ?? RTP_MODES[0]);

  // Custom dropdown (a native <select> can't show a description per option, and
  // shows the same text open and closed). A button + popup listbox lets the
  // closed trigger stay compact while the open list carries each mode's blurb.
  let rtpMenuOpen = $state(false);
  let rtpDropdownEl = $state<HTMLDivElement>();
  function selectRtpMode(value: RtpMode) {
    rtpMode = value;
    rtpMenuOpen = false;
  }
  // Close when clicking away or pressing Escape. pointerdown fires before the
  // trigger's click, but a click inside is contained, so it never self-closes.
  function onDocPointerDown(e: PointerEvent) {
    if (rtpMenuOpen && rtpDropdownEl && !rtpDropdownEl.contains(e.target as Node)) rtpMenuOpen = false;
  }
  function onDocKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') rtpMenuOpen = false;
  }

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

  async function saveAirplay(e?: Event) {
    e?.preventDefault();
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

  async function saveRtp(e?: Event) {
    e?.preventDefault();
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

<svelte:window onpointerdown={onDocPointerDown} onkeydown={onDocKeydown} />

<div class="card">
  <div class="card-head">
    <h2>AirPlay-receive source</h2>
    {#if airplay.name}
      <button class="toggle off" type="button" onclick={disableAirplay} disabled={busy}>Disable</button>
    {:else}
      <button class="toggle on" type="button" onclick={() => saveAirplay()} disabled={busy || !airplayName.trim()}>Enable</button>
    {/if}
  </div>
  <p class="card-sub">
    A single AirPlay target that phones and PCs can cast into. Give it a service name and Save, then use
    the button in the corner to turn it on or off. Encryption compatibility and takeover policy live under
    Advanced.
  </p>
  <form onsubmit={saveAirplay}>
    <div class="field">
      <label for="ap-name">Service name</label>
      <input id="ap-name" type="text" bind:value={airplayName} placeholder="PipeWire Router" />
    </div>

    <details class="advanced">
      <summary>Advanced</summary>
      <div class="row">
        <div class="field">
          <label for="ap-latency">Jitter buffer (ms)</label>
          <input id="ap-latency" type="number" min="20" max="2000" step="10" bind:value={airplayLatency} placeholder="150" />
          <span class="hint">Raise if playback stutters — trades latency for smoother audio.</span>
        </div>
        <div class="field grow">
          <label class="check">
            <input id="ap-auth" type="checkbox" bind:checked={airplayAuthSetup} />
            Auth-setup
          </label>
          <span class="hint">Advertises the auth-setup encryption mode (et=0,4). Enable only if a sender refuses to connect unencrypted — it broadens compatibility but changes how PipeWire senders negotiate.</span>
        </div>
        <div class="field grow">
          <label class="check">
            <input id="ap-takeover" type="checkbox" bind:checked={airplayPreventTakeover} onchange={savePreventTakeover} />
            Prevent takeover
          </label>
          <span class="hint">Refuse a new sender while one is already streaming (applied live, no restart).</span>
        </div>
      </div>
    </details>

    <div class="status-bar">
      <p class="status">
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
      {#if airplay.name}
        <div class="meter" title="Input level {pct(airplayPeak)}%" aria-label="input level">
          <div class="meter-fill" style="width:{pct(airplayPeak)}%"></div>
        </div>
      {/if}
      <div class="status-actions">
        <button class="ghost" type="button" onclick={() => { showManage = true; refreshClients(); }}>
          Manage connections{#if clients.length}&nbsp;({clients.length}){/if}
        </button>
        <button class="primary" type="submit" disabled={busy}>Save</button>
      </div>
    </div>
  </form>
</div>

{#if showManage}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="modal-backdrop" onclick={() => (showManage = false)}>
    <div class="modal-card card" onclick={(e) => e.stopPropagation()}>
      <div class="card-head">
        <h2>AirPlay connections</h2>
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
  <div class="card-head">
    <h2>RTP-receive source</h2>
    {#if rtp.enabled}
      <button class="toggle off" type="button" onclick={disableRtp} disabled={busy}>Disable</button>
    {:else}
      <button class="toggle on" type="button" onclick={() => saveRtp()} disabled={busy}>Enable</button>
    {/if}
  </div>
  <p class="card-sub">
    Receives an RTP/UDP audio stream and exposes it as a routable source. Feed it from the ESP32
    Bluetooth bridge, a Raspberry Pi Bluetooth bridge, or any PipeWire installation running
    <code>module-rtp-sink</code> — they all land on the same <code>bt-bridge-rtp</code> node, so the
    add-on needs no change to accept either. The listen port must match the port the sender targets.
  </p>
  <form onsubmit={saveRtp}>
    <div class="field">
      <label for="rtp-port">Listen port</label>
      <input id="rtp-port" type="number" min="1" max="65535" bind:value={rtpPort} placeholder="46000" />
    </div>
    <div class="field">
      <span class="group-label" id="rtp-source-label">Source</span>
      <div class="rtp-source-row">
        <div class="dropdown" bind:this={rtpDropdownEl}>
          <button
            type="button"
            class="dd-trigger"
            aria-haspopup="listbox"
            aria-expanded={rtpMenuOpen}
            aria-labelledby="rtp-source-label"
            onclick={() => (rtpMenuOpen = !rtpMenuOpen)}
          >
            <span>{rtpModeInfo.label}</span>
            <span class="caret" aria-hidden="true">▾</span>
          </button>
          {#if rtpMenuOpen}
            <ul class="dd-menu" role="listbox" aria-labelledby="rtp-source-label">
              {#each RTP_MODES as m (m.value)}
                <li role="option" aria-selected={m.value === rtpMode}>
                  <button type="button" class="dd-item" class:active={m.value === rtpMode} onclick={() => selectRtpMode(m.value)}>
                    <strong>{m.label}</strong>
                    <span class="muted">{m.desc}</span>
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
        {#if rtpMode === 'multicast'}
          <input
            id="rtp-mcast"
            class="mcast-input"
            type="text"
            bind:value={rtpMulticastAddr}
            placeholder="239.255.42.42"
            aria-label="Multicast group address"
          />
        {/if}
      </div>
      <span class="hint">{rtpModeInfo.desc}</span>
    </div>

    <details class="advanced">
      <summary>Advanced</summary>
      <div class="field">
        <label for="rtp-latency">Jitter buffer (ms)</label>
        <input id="rtp-latency" type="number" min="20" max="2000" step="10" bind:value={rtpLatency} placeholder="200" />
        <span class="hint">Raise if a weak-signal bridge stutters — trades latency for dropout tolerance.</span>
      </div>
    </details>

    <div class="status-bar">
      <p class="status">
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
      {#if rtp.enabled}
        <div class="meter" title="Input level {pct(rtpPeak)}%" aria-label="input level">
          <div class="meter-fill" style="width:{pct(rtpPeak)}%"></div>
        </div>
      {/if}
      <div class="status-actions">
        <button class="primary" type="submit" disabled={busy}>Save</button>
      </div>
    </div>
  </form>
</div>

<style>
  /* Card header: title on the left, the master enable/disable toggle top-right. */
  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
  }
  .card-head h2 {
    margin: 0;
  }

  /* Master on/off toggle — green to enable, red to disable. */
  button.toggle {
    color: #fff;
    border-color: transparent;
    flex: 0 0 auto;
  }
  button.toggle.on {
    background: var(--success-color);
  }
  button.toggle.off {
    background: var(--error-color);
  }

  /* Collapsible advanced block, hidden by default. */
  details.advanced {
    margin-top: 12px;
    border-top: 1px solid var(--divider-color);
    padding-top: 12px;
  }
  details.advanced > summary {
    cursor: pointer;
    font-size: 0.8rem;
    font-weight: 500;
    color: var(--secondary-text-color);
    list-style: none;
    user-select: none;
  }
  details.advanced > summary::-webkit-details-marker {
    display: none;
  }
  details.advanced > summary::before {
    content: '▸';
    display: inline-block;
    margin-right: 6px;
    transition: transform 0.15s;
  }
  details.advanced[open] > summary::before {
    transform: rotate(90deg);
  }
  details.advanced > summary + * {
    margin-top: 12px;
  }
  details.advanced .field {
    margin-bottom: 0;
  }
  label.check {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 0;
  }
  label.check input {
    width: auto;
  }
  .hint {
    display: block;
    margin-top: 4px;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }

  /* Bottom bar: live status on the left, Save (and Manage) on the right. */
  .status-bar {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 16px;
    padding-top: 12px;
    border-top: 1px solid var(--divider-color);
  }
  .status-bar .status {
    flex: 1 1 auto;
    margin: 0;
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
  .status-bar .status-actions {
    display: flex;
    gap: 8px;
    flex: 0 0 auto;
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
  /* Compact source picker: one line (dropdown + optional group address), the
     selected mode's description on the line below (rendered as .hint). */
  .rtp-source-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .dropdown {
    position: relative;
    flex: 0 0 auto;
  }
  .dd-trigger {
    display: inline-flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    min-width: 12rem;
    padding: 6px 10px;
    background: var(--card-background-color, var(--ha-card-background, #fff));
    border: 1px solid var(--divider-color);
    border-radius: 6px;
    color: inherit;
    font: inherit;
    cursor: pointer;
    text-align: left;
  }
  .dd-trigger .caret {
    color: var(--secondary-text-color);
    font-size: 0.7rem;
  }
  .dd-menu {
    position: absolute;
    z-index: 20;
    top: calc(100% + 4px);
    left: 0;
    min-width: 20rem;
    max-width: min(28rem, 90vw);
    margin: 0;
    padding: 4px;
    list-style: none;
    background: var(--card-background-color, var(--ha-card-background, #fff));
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    box-shadow: 0 6px 24px rgba(0, 0, 0, 0.18);
  }
  .dd-item {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    width: 100%;
    padding: 8px 10px;
    background: none;
    border: none;
    border-radius: 6px;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .dd-item:hover,
  .dd-item:focus-visible {
    background: color-mix(in srgb, var(--primary-color) 12%, transparent);
  }
  .dd-item.active {
    background: color-mix(in srgb, var(--primary-color) 18%, transparent);
  }
  .dd-item strong {
    font-weight: 600;
  }
  .dd-item .muted {
    font-size: 0.8rem;
    line-height: 1.35;
  }
  .mcast-input {
    flex: 1 1 12rem;
    max-width: 16rem;
    margin: 0;
  }

  /* Live input-level meter — same look as the routing-graph meter. */
  .meter {
    flex: 1 1 100px;
    max-width: 220px;
    height: 4px;
    border-radius: 2px;
    background: color-mix(in srgb, var(--secondary-text-color) 20%, transparent);
    overflow: hidden;
  }
  .meter-fill {
    height: 100%;
    background: var(--success-color, #2e7d32);
    transition: width 120ms linear;
  }
</style>
