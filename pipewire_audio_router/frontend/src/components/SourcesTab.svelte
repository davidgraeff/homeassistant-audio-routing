<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { routing } from '../lib/routing';
  import { run } from '../lib/toast';
  import { align } from '../lib/align.svelte';
  import RtpSenderDocs from './RtpSenderDocs.svelte';
  import AlignDocs from './AlignDocs.svelte';
  import AlignPanel from './AlignPanel.svelte';
  import type {
    AirplayClient,
    AirplaySourceCfg,
    BridgeInfo,
    RtpSourceCfg,
    SourceKind,
    SourceView,
  } from '../lib/types';

  // The set of configured input sources (AirPlay-receive + RTP-receive). Loaded
  // from the daemon's /api/sources collection; the user adds / edits / removes
  // entries of either kind, and each shows up in the routing matrix by its
  // node_name automatically. (The CRUD endpoints land in a later phase, so
  // these calls 404 until then — that's expected.)
  let sources = $state<SourceView[]>([]);
  // Bluetooth→RTP bridges discovered over mDNS that no source is listening for
  // yet. Same response as `sources`, so the two can never disagree about which
  // bridge is already adopted.
  let discoveredBridges = $state<BridgeInfo[]>([]);
  let loading = $state(true);
  let busy = $state(false);

  async function refresh() {
    loading = true;
    try {
      const res = await api.listSources();
      sources = res.sources;
      discoveredBridges = res.discovered_bridges ?? [];
    } catch {
      sources = [];
      discoveredBridges = [];
    }
    loading = false;
    // Pull the client lists for whatever AirPlay sources we now know about.
    await refreshAllClients();
  }

  // ---- AirPlay senders (per-source clients) --------------------------------
  // Each AirPlay source owns one receiver with its own remembered-sender list.
  // We keep the lists keyed by source id, poll them on a light interval while
  // this tab is mounted, and refresh after each management action. RTP sources
  // have no such list. The list itself lives in the expanded card body; the
  // header only carries the connected count.
  let clientsBySource = $state<Record<string, AirplayClient[]>>({});

  function clientsFor(id: string): AirplayClient[] {
    const list = clientsBySource[id] ?? [];
    // Connected senders first, then by most-recent connection.
    return [...list].sort(
      (a, b) => Number(b.connected) - Number(a.connected) || b.last_connected - a.last_connected,
    );
  }
  const connectedCount = (id: string) => (clientsBySource[id] ?? []).filter((c) => c.connected).length;

  function clientLabel(c: AirplayClient): string {
    return c.name ?? c.addr ?? c.key;
  }
  function formatWhen(unixSecs: number): string {
    if (!unixSecs) return 'never';
    return new Date(unixSecs * 1000).toLocaleString();
  }

  async function refreshClients(id: string) {
    try {
      clientsBySource = { ...clientsBySource, [id]: await api.listSourceClients(id) };
    } catch {
      /* keep last-known for this source */
    }
  }
  async function refreshAllClients() {
    await Promise.all(sources.filter((s) => s.kind === 'airplay').map((s) => refreshClients(s.id)));
  }

  async function forgetClient(id: string, c: AirplayClient) {
    if (await run(() => api.forgetSourceClient(id, c.key), `Forgot '${clientLabel(c)}'`)) {
      await refreshClients(id);
    }
  }
  async function banClient(id: string, c: AirplayClient, banned: boolean) {
    const msg = banned ? `Banned '${clientLabel(c)}'` : `Unbanned '${clientLabel(c)}'`;
    if (await run(() => api.banSourceClient(id, c.key, banned), msg)) {
      await refreshClients(id);
    }
  }
  async function setPriority(id: string, c: AirplayClient, priority: number) {
    if (await run(() => api.setSourceClientPriority(id, c.key, priority), `Priority ${priority} for '${clientLabel(c)}'`)) {
      await refreshClients(id);
    }
  }
  async function disconnectClient(id: string, c: AirplayClient) {
    if (await run(() => api.disconnectSourceClient(id, c.key), `Disconnecting '${clientLabel(c)}'`)) {
      await refreshClients(id);
    }
  }

  onMount(() => {
    refresh();
    // Light poll so connect/disconnect/ban shows up without a manual reload.
    const timer = setInterval(refreshAllClients, 5000);
    // Alignment lives on this page (a sync group is identified by its source):
    // attach loads the session + alignable groups and, on unmount, stops any
    // session so speakers aren't left muted with the click looping.
    const detachAlign = align.attach();
    return () => {
      clearInterval(timer);
      detachAlign();
    };
  });

  // Live input-level meters. Subscribing to the `routing` store opens the same
  // WebSocket the routing matrix uses, whose snapshots carry each present
  // source's current peak (0.0–1.0). Match by the daemon's stable source node
  // name — every SourceView carries its `node_name`.
  const liveSources = $derived($routing.matrix.sources);
  const pct = (peak: number) => Math.min(100, Math.round(peak * 100));
  function peakFor(nodeName: string): number {
    return liveSources.find((s) => s.node_name === nodeName)?.peak ?? 0;
  }

  function kindLabel(kind: SourceKind): string {
    return kind === 'airplay' ? 'AirPlay' : 'RTP';
  }

  // ---- RTP "Source" mode (same mapping as the legacy single RTP panel) -----
  // Two of the three modes pin source.ip to 0.0.0.0 and differ only in
  // sess.ignore-ssrc; the multicast mode reveals the group-address field.
  type RtpMode = 'all' | 'multicast' | 'single';
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

  // A non-empty, non-0.0.0.0 source.ip means the receiver joined a multicast
  // group; otherwise ignore-ssrc distinguishes "accept all" from "single".
  function deriveRtpMode(addr: string, ignoreSsrc: boolean): RtpMode {
    if (addr && addr !== '0.0.0.0') return 'multicast';
    return ignoreSsrc ? 'all' : 'single';
  }
  // Map the "Source" mode to the two backend knobs.
  function rtpModeToParams(): { sourceAddr: string; ignoreSsrc: boolean } {
    if (rtpMode === 'multicast') return { sourceAddr: rtpMulticastAddr.trim() || '239.255.42.42', ignoreSsrc: true };
    if (rtpMode === 'single') return { sourceAddr: '0.0.0.0', ignoreSsrc: false };
    return { sourceAddr: '0.0.0.0', ignoreSsrc: true };
  }

  // Custom dropdown for the RTP mode (a native <select> can't show a per-option
  // description). Only one form is open at a time, so a single open-state suffices.
  let rtpMenuOpen = $state(false);
  let rtpDropdownEl = $state<HTMLDivElement>();
  function selectRtpMode(value: RtpMode) {
    rtpMode = value;
    rtpMenuOpen = false;
  }
  function onDocPointerDown(e: PointerEvent) {
    if (rtpMenuOpen && rtpDropdownEl && !rtpDropdownEl.contains(e.target as Node)) rtpMenuOpen = false;
  }
  function onDocKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') rtpMenuOpen = false;
  }

  // ---- Add / edit form -----------------------------------------------------
  // One editor at a time: either adding a new source of a chosen kind, or
  // editing an existing one (id + kind are immutable, so the kind is fixed for
  // the lifetime of the form). Field state is shared between add and edit and
  // reset/seeded whenever the form opens.
  //
  // For configured sources this doubles as the card's collapse state: a source
  // card is expanded exactly while it is the one being edited, so the header
  // chevron seeds/tears down the form (there is no separate "Edit" button and
  // no "Cancel" — collapsing the card is the cancel).
  type FormMode = { type: 'add'; kind: SourceKind } | { type: 'edit'; id: string; kind: SourceKind };
  let form = $state<FormMode | null>(null);

  let formLabel = $state('');
  // AirPlay fields
  let apLatency = $state(150);
  let apAuthSetup = $state(false);
  let apPreventTakeover = $state(true);
  let apPort = $state(0); // 0 = let the daemon allocate the RTSP port
  // RTP fields
  let rtpPort = $state(46000);
  let rtpLatency = $state(200);
  let rtpRate = $state(48000);
  let rtpMode = $state<RtpMode>('all');
  let rtpMulticastAddr = $state('239.255.42.42');

  function resetAirplayFields(cfg?: AirplaySourceCfg | null) {
    apLatency = cfg?.latency_msec ?? 150;
    apAuthSetup = cfg?.auth_setup ?? false;
    apPreventTakeover = cfg?.prevent_takeover ?? true;
    apPort = cfg?.port ?? 0;
  }
  function resetRtpFields(cfg?: RtpSourceCfg | null) {
    rtpPort = cfg?.port ?? 46000;
    rtpLatency = cfg?.latency_msec ?? 200;
    rtpRate = cfg?.rate ?? 48000;
    rtpMode = cfg ? deriveRtpMode(cfg.source_addr, cfg.ignore_ssrc) : 'all';
    // Keep the group field populated with the stored group (so it round-trips)
    // but never with 0.0.0.0 — the field only shows in multicast mode.
    if (cfg && rtpMode === 'multicast') rtpMulticastAddr = cfg.source_addr;
    else rtpMulticastAddr = '239.255.42.42';
  }

  function openAdd(kind: SourceKind) {
    form = { type: 'add', kind };
    formLabel = '';
    rtpMenuOpen = false;
    if (kind === 'airplay') resetAirplayFields();
    else resetRtpFields();
  }
  function openEdit(s: SourceView) {
    form = { type: 'edit', id: s.id, kind: s.kind };
    formLabel = s.label;
    rtpMenuOpen = false;
    if (s.kind === 'airplay') resetAirplayFields(s.airplay);
    else resetRtpFields(s.rtp);
  }
  function closeForm() {
    form = null;
    rtpMenuOpen = false;
  }
  // Cards start collapsed; expanding one opens its editor (and implicitly
  // collapses whichever card was open before).
  const isExpanded = (s: SourceView) => !!form && form.type === 'edit' && form.id === s.id;
  function toggleCard(s: SourceView) {
    if (isExpanded(s)) closeForm();
    else openEdit(s);
  }

  function airplayBody(): Partial<AirplaySourceCfg> {
    const body: Partial<AirplaySourceCfg> = {
      latency_msec: apLatency,
      auth_setup: apAuthSetup,
      prevent_takeover: apPreventTakeover,
    };
    // Only pin a port when the user set one; otherwise let the daemon allocate.
    if (apPort > 0) body.port = apPort;
    return body;
  }
  function rtpBody(): Partial<RtpSourceCfg> {
    const { sourceAddr, ignoreSsrc } = rtpModeToParams();
    return {
      port: rtpPort,
      latency_msec: rtpLatency,
      source_addr: sourceAddr,
      ignore_ssrc: ignoreSsrc,
      rate: rtpRate,
    };
  }

  async function save(e?: Event) {
    e?.preventDefault();
    const f = form;
    if (!f) return;
    const label = formLabel.trim();
    if (!label) return;
    busy = true;
    let ok = false;
    if (f.type === 'add') {
      const body =
        f.kind === 'airplay'
          ? { label, kind: f.kind, airplay: airplayBody() }
          : { label, kind: f.kind, rtp: rtpBody() };
      ok = await run(() => api.addSource(body), `Added ${kindLabel(f.kind)} source '${label}'`);
    } else {
      const body = f.kind === 'airplay' ? { label, airplay: airplayBody() } : { label, rtp: rtpBody() };
      ok = await run(() => api.updateSource(f.id, body), `Updated source '${label}'`);
    }
    busy = false;
    if (ok) {
      closeForm();
      await refresh();
    }
  }

  async function remove(s: SourceView) {
    if (!confirm(`Remove source '${s.label}'? Its routing will be forgotten.`)) return;
    if (form && form.type === 'edit' && form.id === s.id) closeForm();
    if (await run(() => api.deleteSource(s.id), `Removed source '${s.label}'`)) await refresh();
  }

  // ---- Discovered Bluetooth bridges ---------------------------------------
  // A bridge advertises the port/rate/destination it *actually sends*, so adopting
  // one copies those over instead of asking the user to retype them — a typo'd
  // port looks exactly like a bridge that isn't sending, which is the single most
  // confusing failure this page can produce.

  /** The RTP source config that receives what `b` transmits. */
  function bridgeRtpBody(b: BridgeInfo): Partial<RtpSourceCfg> {
    const multicast = /^(2(2[4-9]|3\d)\.)/.test(b.rtp_dest) || b.rtp_dest.toLowerCase().startsWith('ff');
    return {
      port: b.rtp_port,
      rate: b.rate,
      latency_msec: 200,
      // Mirror the sender: a multicast bridge needs the group joined (and
      // `ignore_ssrc`, since a shared group may carry more than one sender);
      // a unicast one just needs the port open to anyone.
      source_addr: multicast ? b.rtp_dest : '0.0.0.0',
      ignore_ssrc: true,
    };
  }

  async function adoptBridge(b: BridgeInfo) {
    busy = true;
    const ok = await run(
      () => api.addSource({ label: b.display_name, kind: 'rtp', rtp: bridgeRtpBody(b) }),
      `Added RTP source '${b.display_name}'`,
    );
    busy = false;
    if (ok) await refresh();
  }

  /** Open the add form prefilled from `b`, for renaming/tweaking before saving. */
  function openAddFromBridge(b: BridgeInfo) {
    openAdd('rtp');
    formLabel = b.display_name;
    const cfg = bridgeRtpBody(b);
    rtpPort = cfg.port ?? 46000;
    rtpRate = cfg.rate ?? 48000;
    rtpLatency = cfg.latency_msec ?? 200;
    rtpMode = deriveRtpMode(cfg.source_addr ?? '0.0.0.0', cfg.ignore_ssrc ?? true);
    if (rtpMode === 'multicast') rtpMulticastAddr = cfg.source_addr ?? '239.255.42.42';
  }

  // ---- Alignment docs ------------------------------------------------------
  // Static document (no per-source parameters), so a simple open flag.
  let alignDocsOpen = $state(false);

  // ---- Sender setup docs (RTP only) ---------------------------------------
  // Fed the port/rate/mode currently in view so the sample configs are
  // copy-pasteable for this install (see RtpSenderDocs.svelte).
  let docsParams = $state<{
    port: number;
    latencyMsec: number;
    rate: number;
    sourceAddr: string;
    ignoreSsrc: boolean;
  } | null>(null);
  // Single entry point, from the tab header — so it's readable *before* any RTP
  // source exists. Whichever settings are most relevant win: the RTP form's live
  // values while one is open, else the first configured RTP source, else the
  // defaults a fresh RTP source would get (never a generic template).
  function openDocs() {
    if (form?.kind === 'rtp') {
      const { sourceAddr, ignoreSsrc } = rtpModeToParams();
      docsParams = { port: rtpPort, latencyMsec: rtpLatency, rate: rtpRate, sourceAddr, ignoreSsrc };
      return;
    }
    const cfg = sources.find((s) => s.kind === 'rtp')?.rtp;
    docsParams = cfg
      ? {
          port: cfg.port,
          latencyMsec: cfg.latency_msec,
          rate: cfg.rate,
          sourceAddr: cfg.source_addr,
          ignoreSsrc: cfg.ignore_ssrc,
        }
      : { port: 46000, latencyMsec: 200, rate: 48000, sourceAddr: '0.0.0.0', ignoreSsrc: true };
  }
</script>

<svelte:window onpointerdown={onDocPointerDown} onkeydown={onDocKeydown} />

<div class="card info">
  <div class="info-head">
    <h2>Input sources</h2>
    <div class="info-actions">
      <button
        class="ghost"
        type="button"
        title="Why speakers on one source drift apart, and how to align them by ear"
        onclick={() => (alignDocsOpen = true)}
      >
        Explain speaker alignment
      </button>
      <button
        class="ghost"
        type="button"
        title="How to turn a PipeWire machine into a sender that feeds an RTP-receive source"
        onclick={openDocs}
      >
        Explain RTP sender setup
      </button>
    </div>
  </div>
  <p class="card-sub">
    The audio this router can receive and route onward. Add as many as you like of two kinds:
    an <strong>AirPlay-receive</strong> endpoint (a target phones and PCs cast into) or an
    <strong>RTP-receive</strong> endpoint (an ESP32 / Raspberry Pi Bluetooth bridge, or any PipeWire
    machine running <code>module-rtp-sink</code>). Each source is independently routable in the matrix.
  </p>
  <p class="card-sub" style="margin-bottom:0">
    A <span class="badge on">present</span> source has a live PipeWire node right now; an
    <span class="badge off">offline</span> one is configured but not currently receiving. Each card also lists the
    speakers currently playing that source — they play off one clock, so that's the set you can
    <strong>align</strong> by ear. Expand a card for its settings, its connected senders, and to remove it.
  </p>
</div>

<!-- Both field groups are shared by the add form and a card's edit form, and only
     one form is ever open at a time — but the id prefix (`idp`) keeps label/input
     pairs unambiguous either way. Everything is on one flat surface: no
     "Advanced" disclosure, since a source has few enough settings that hiding
     two of them cost more than it saved. Fields that belong together sit in a
     `.row.fields`, which wraps to one per line on a narrow screen. -->

<!-- AirPlay field group (references the ap* form state). -->
{#snippet airplayFields(idp: string)}
  <div class="row fields">
    <div class="field grow">
      <label for="{idp}-label">Name</label>
      <input id="{idp}-label" type="text" bind:value={formLabel} placeholder="Kitchen AirPlay" />
      <span class="hint">The service name phones and PCs see when casting.</span>
    </div>
    <div class="field narrow">
      <label for="{idp}-latency">Jitter buffer (ms)</label>
      <input id="{idp}-latency" type="number" min="20" max="2000" step="10" bind:value={apLatency} placeholder="150" />
      <span class="hint">Raise if playback stutters — trades latency for smoother audio.</span>
    </div>
  </div>
  <div class="row fields">
    <div class="field grow">
      <label class="check">
        <input type="checkbox" bind:checked={apAuthSetup} />
        Auth-setup
      </label>
      <span class="hint">Advertises the auth-setup encryption mode (et=0,4). Enable only if a sender refuses to connect unencrypted — it broadens compatibility but changes how PipeWire senders negotiate.</span>
    </div>
    <div class="field grow">
      <label class="check">
        <input type="checkbox" bind:checked={apPreventTakeover} />
        Prevent takeover
      </label>
      <span class="hint">Refuse a new sender while one is already streaming.</span>
    </div>
  </div>
  <div class="field narrow last">
    <label for="{idp}-ap-port">RTSP port</label>
    <input id="{idp}-ap-port" type="number" min="0" max="65535" bind:value={apPort} placeholder="auto" />
    <span class="hint">The AirPlay control port. Leave 0 to let the router allocate a free one (starting at 5000) and keep it stable across restarts.</span>
  </div>
{/snippet}

<!-- RTP field group (references the rtp* form state). -->
{#snippet rtpFields(idp: string)}
  <div class="row fields">
    <div class="field grow">
      <label for="{idp}-label">Name</label>
      <input id="{idp}-label" type="text" bind:value={formLabel} placeholder="Bluetooth Bridge" />
    </div>
    <div class="field narrow">
      <label for="{idp}-rtp-port">Listen port</label>
      <input id="{idp}-rtp-port" type="number" min="1" max="65535" bind:value={rtpPort} placeholder="46000" />
      <span class="hint">Must match the port the sender targets. Two enabled RTP sources can't share a port.</span>
    </div>
  </div>
  <div class="field">
    <span class="group-label" id="{idp}-rtp-source-label">Source</span>
    <div class="rtp-source-row">
      <div class="dropdown" bind:this={rtpDropdownEl}>
        <button
          type="button"
          class="dd-trigger"
          aria-haspopup="listbox"
          aria-expanded={rtpMenuOpen}
          aria-labelledby="{idp}-rtp-source-label"
          onclick={() => (rtpMenuOpen = !rtpMenuOpen)}
        >
          <span>{rtpModeInfo.label}</span>
          <span class="caret" aria-hidden="true">▾</span>
        </button>
        {#if rtpMenuOpen}
          <ul class="dd-menu" role="listbox" aria-labelledby="{idp}-rtp-source-label">
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
  <div class="row fields last">
    <div class="field narrow">
      <label for="{idp}-rtp-latency">Jitter buffer (ms)</label>
      <input id="{idp}-rtp-latency" type="number" min="20" max="2000" step="10" bind:value={rtpLatency} placeholder="200" />
      <span class="hint">Raise if a weak-signal bridge stutters — trades latency for dropout tolerance.</span>
    </div>
    <div class="field narrow">
      <label for="{idp}-rtp-rate">Sample rate</label>
      <select id="{idp}-rtp-rate" bind:value={rtpRate}>
        <option value={48000}>48 kHz (recommended)</option>
        <option value={44100}>44.1 kHz</option>
      </select>
      <span class="hint">Must match the sender. 48 kHz keeps the whole path at the router's native rate (no resample).</span>
    </div>
  </div>
{/snippet}

{#if loading}
  <div class="card"><p class="empty" style="padding:0">Loading…</p></div>
{:else}
  {#if sources.length === 0 && !(form && form.type === 'add')}
    <div class="card">
      <p class="empty" style="padding:0">No input sources yet. Add an AirPlay or RTP receiver below.</p>
    </div>
  {/if}

  {#each sources as s (s.id)}
    <article class="card src-card" class:offline={!s.present} class:collapsed={!isExpanded(s)}>
      <header class="src-head">
        <button
          class="collapse-toggle"
          type="button"
          aria-expanded={isExpanded(s)}
          title={isExpanded(s) ? 'Hide settings' : 'Show settings'}
          onclick={() => toggleCard(s)}
        >
          <span class="chevron">▶</span>
        </button>
        <div class="src-title">
          <h3>{s.label}</h3>
          <div class="src-badges">
            <span class="badge">{kindLabel(s.kind)}</span>
            <span class="badge {s.present ? 'on' : 'off'}">{s.present ? 'present' : 'offline'}</span>
            {#if s.kind === 'airplay' && connectedCount(s.id)}
              <span class="badge on" title="Senders streaming into this receiver right now">
                {connectedCount(s.id)}&nbsp;connected
              </span>
            {/if}
            {#if s.bridge}
              <!-- Which discovered Pi feeds this source. Visible collapsed too:
                   it identifies the sender, which nothing else on this page does. -->
              <span
                class="badge {s.bridge.diag_ok ? 'on' : ''}"
                title="Discovered Bluetooth bridge sending to this source: {s.bridge.hostname}{s.bridge.addr
                  ? ` (${s.bridge.addr})`
                  : ''}{s.bridge.diag_ok ? '' : ' — diagnostics page not answering'}"
              >
                via&nbsp;{s.bridge.display_name}
              </span>
            {/if}
            <code class="muted node-name" title="Routing node name">{s.node_name}</code>
          </div>
        </div>
        {#if s.present}
          <div class="meter" title="Input level {pct(peakFor(s.node_name))}%" aria-label="input level">
            <div class="meter-fill" style="width:{pct(peakFor(s.node_name))}%"></div>
          </div>
        {/if}
      </header>

      <!-- The sync group this source feeds + its alignment session. Shown
           collapsed as well as expanded: it's live state, not a setting. -->
      <AlignPanel sourceNodeName={s.node_name} />

      {#if isExpanded(s)}
        <form class="src-form" onsubmit={save}>
          {#if s.kind === 'airplay'}
            {@render airplayFields('edit')}
          {:else}
            {@render rtpFields('edit')}
          {/if}
          <div class="form-actions">
            <span class="spacer"></span>
            {#if s.bridge?.diag_ok && s.bridge.diag_url}
              <!-- Only when the daemon's probe found the bridge's diagnostics app
                   answering: the mDNS advert is written by setup_pi_bridge.py and
                   outlives any run of that app, so the advert alone would produce
                   dead links. Opens the Pi directly, so it needs a browser on the
                   same LAN — hence the explicit note in the title. -->
              <a
                class="ghost btn-link"
                href={s.bridge.diag_url}
                target="_blank"
                rel="noopener noreferrer"
                title="Open the live Bluetooth diagnostics page on {s.bridge.display_name} ({s.bridge.hostname}) — served by that Pi, so your browser must be on the same network"
              >
                Show diagnostics ↗
              </a>
            {/if}
            <button class="ghost danger" type="button" onclick={() => remove(s)} disabled={busy}>Remove</button>
            <button class="primary" type="submit" disabled={busy || !formLabel.trim()}>Save</button>
          </div>
        </form>
      {/if}

      {#if s.kind === 'airplay' && isExpanded(s)}
        {@const list = clientsFor(s.id)}
        <div class="clients-panel">
          <p class="hint" style="margin-top:0">
            Senders that have connected to this AirPlay receiver. <strong>Ban</strong> refuses a client's future
            connections (matched by name if known, else by IP; it doesn't drop a live stream — use
            <strong>Disconnect</strong> for that). <strong>Priority</strong> arbitrates takeover: a connecting
            sender with a strictly higher priority takes over the current one. <strong>Forget</strong> drops a
            previously-seen client (only when not connected).
          </p>
          {#if list.length === 0}
            <p class="empty" style="padding:0">No sender has connected to this receiver yet.</p>
          {:else}
            <table class="clients">
              <thead>
                <tr><th>Client</th><th>Address</th><th>Last connected</th><th>Priority</th><th></th></tr>
              </thead>
              <tbody>
                {#each list as c (c.key)}
                  <tr class:connected={c.connected}>
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
                        onchange={(e) => setPriority(s.id, c, Number((e.currentTarget as HTMLInputElement).value) || 0)}
                      />
                    </td>
                    <td style="text-align:right; white-space:nowrap">
                      {#if c.connected}
                        <button class="ghost danger" type="button" title="Drop this client's current connection" onclick={() => disconnectClient(s.id, c)}>
                          Disconnect
                        </button>
                      {/if}
                      {#if c.banned}
                        <button class="ghost" type="button" title="Allow this client again" onclick={() => banClient(s.id, c, false)}>
                          Unban
                        </button>
                      {:else}
                        <button class="ghost danger" type="button" title="Refuse this client's future connections" onclick={() => banClient(s.id, c, true)}>
                          Ban
                        </button>
                      {/if}
                      <button
                        class="ghost danger"
                        type="button"
                        disabled={c.connected}
                        title={c.connected ? 'Disconnect before forgetting' : 'Forget this client'}
                        onclick={() => forgetClient(s.id, c)}
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
      {/if}
    </article>
  {/each}

  {#if form && form.type === 'add'}
    <article class="card src-card add-form">
      <header class="src-head">
        <div class="src-title">
          <h3>New {kindLabel(form.kind)} source</h3>
        </div>
      </header>
      <form class="src-form" onsubmit={save}>
        {#if form.kind === 'airplay'}
          {@render airplayFields('add')}
        {:else}
          {@render rtpFields('add')}
        {/if}
        <div class="form-actions">
          <span class="spacer"></span>
          <button class="ghost" type="button" onclick={closeForm} disabled={busy}>Cancel</button>
          <button class="primary" type="submit" disabled={busy || !formLabel.trim()}>Add source</button>
        </div>
      </form>
    </article>
  {:else}
    <div class="card add-bar">
      <span class="add-label">Add source:</span>
      <button class="ghost" type="button" onclick={() => openAdd('airplay')} disabled={busy}>+ AirPlay receiver</button>
      <button class="ghost" type="button" onclick={() => openAdd('rtp')} disabled={busy}>+ RTP receiver</button>
    </div>
  {/if}

  <!-- Bridges that announced themselves but that nothing is listening for. Only
       rendered when there are any, so a LAN without bridges sees no dead UI. -->
  {#if discoveredBridges.length}
    <section class="card discovered">
      <h2>Discovered Bluetooth bridges</h2>
      <p class="note" style="margin-top:0">
        These Raspberry Pi bridges announced themselves on the network but nothing here is receiving
        them yet. <strong>Add as RTP source</strong> copies the port, rate and destination they said
        they transmit, so the two ends can't disagree by typo. Set up with
        <code>firmware/pi-bridge/setup_pi_bridge.py</code>.
      </p>
      <table class="bridges">
        <thead>
          <tr><th>Bridge</th><th>Sends to</th><th>Format</th><th></th></tr>
        </thead>
        <tbody>
          {#each discoveredBridges as b (b.fullname)}
            <tr>
              <td>
                <strong>{b.display_name}</strong>
                <span class="muted">{b.hostname}{b.addr ? ` · ${b.addr}` : ''}</span>
              </td>
              <td class="muted"><code>{b.rtp_dest}:{b.rtp_port}</code></td>
              <td class="muted">{(b.rate / 1000).toFixed(1)} kHz · {b.channels}&nbsp;ch</td>
              <td style="text-align:right; white-space:nowrap">
                {#if b.diag_ok && b.diag_url}
                  <a class="ghost btn-link" href={b.diag_url} target="_blank" rel="noopener noreferrer"
                     title="Open this bridge's live Bluetooth diagnostics page">Diagnostics ↗</a>
                {/if}
                <button class="ghost" type="button" disabled={busy}
                        title="Open the add form with this bridge's settings, to rename or adjust first"
                        onclick={() => openAddFromBridge(b)}>Review…</button>
                <button class="primary" type="button" disabled={busy}
                        title="Create an RTP source that receives this bridge"
                        onclick={() => adoptBridge(b)}>Add as RTP source</button>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </section>
  {/if}
{/if}

{#if alignDocsOpen}
  <AlignDocs onClose={() => (alignDocsOpen = false)} />
{/if}

{#if docsParams}
  <RtpSenderDocs
    port={docsParams.port}
    latencyMsec={docsParams.latencyMsec}
    rate={docsParams.rate}
    sourceAddr={docsParams.sourceAddr}
    ignoreSsrc={docsParams.ignoreSsrc}
    onClose={() => (docsParams = null)}
  />
{/if}

<style>
  /* Tab header: title with the sender-setup docs button beside it, so the
     document is reachable before the first RTP source exists. */
  .info-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }
  .info-head h2 {
    margin: 0;
  }
  .info-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }

  /* One card per source; offline ones dim like the outputs list. */
  .src-card.offline {
    opacity: 0.6;
  }
  /* Collapsed cards are a single compact line (same shape as the outputs
     list): no body, tighter padding, and nothing wraps — the node name
     truncates instead. */
  .src-card.collapsed {
    padding-top: 12px;
    padding-bottom: 12px;
  }
  .src-card.collapsed .src-head,
  .src-card.collapsed .src-title,
  .src-card.collapsed .src-badges {
    flex-wrap: nowrap;
  }
  .src-card.collapsed .src-head {
    overflow: hidden;
  }
  .src-card.collapsed .src-title h3,
  .src-card.collapsed .node-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .src-head {
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
  .src-title {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    min-width: 0;
    flex: 1 1 auto;
  }
  .src-title h3 {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 500;
  }
  .src-badges {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
    min-width: 0;
  }
  .node-name {
    font-size: 0.75rem;
  }
  button.ghost.danger {
    color: var(--error-color, #db4437);
  }

  /* The inline add/edit form under a card header. */
  .src-form {
    margin-top: 14px;
    padding-top: 14px;
    border-top: 1px solid var(--divider-color);
  }
  .form-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 12px;
  }
  .form-actions .spacer {
    flex: 1 1 auto;
  }

  /* The bottom "Add source:" bar. */
  .add-bar {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .add-label {
    color: var(--secondary-text-color);
    font-size: 0.9rem;
  }

  /* A row of labelled fields: two equal columns while there's room for them,
     one per line below that (a grid rather than `.row`'s flex, so the columns
     line up across rows and the labels don't stagger with the hints). */
  .row.fields {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr));
    align-items: start;
    gap: 0 16px;
  }
  .row.fields > .field {
    margin-bottom: 12px;
  }
  /* A port, a buffer size or a rate doesn't need the whole column — but its
     hint does, so the control shrinks, not the field. */
  .field.narrow input {
    max-width: 12rem;
  }
  .field.narrow select {
    max-width: 15rem; /* wide enough for "48 kHz (recommended)" */
  }
  /* The form's last field group sits directly above the action buttons. */
  .field.last,
  .row.fields.last > .field {
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

  /* Compact RTP source picker: dropdown + optional group address on one line,
     the selected mode's description below (as .hint). */
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

  /* Per-source AirPlay senders sub-panel. */
  .clients-panel {
    margin-top: 14px;
    padding-top: 14px;
    border-top: 1px solid var(--divider-color);
    overflow-x: auto;
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
  table.clients tr.connected td {
    background: color-mix(in srgb, var(--success-color, #2e7d32) 8%, transparent);
  }
  table.clients .badge {
    margin-left: 0.4rem;
  }
  input.prio {
    width: 4rem;
    margin: 0;
  }

  /* Discovered-bridge listing. Same table shape as the AirPlay client list, and
     `.btn-link` makes an <a> sit level with the buttons beside it — it has to be
     an anchor, not a button, because it opens the Pi's own page in a new tab. */
  table.bridges {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.9rem;
  }
  table.bridges th,
  table.bridges td {
    text-align: left;
    padding: 0.4rem 0.5rem;
    border-bottom: 1px solid var(--divider-color);
    vertical-align: middle;
  }
  table.bridges th {
    font-weight: 600;
    color: var(--secondary-text-color);
  }
  table.bridges .muted {
    display: inline-block;
    margin-left: 0.4rem;
  }
  .discovered {
    overflow-x: auto;
  }
  a.btn-link {
    display: inline-block;
    text-decoration: none;
    /* Buttons in this UI are styled globally by element; an <a> needs the box
       metrics restated so it lines up with the Remove/Save row. */
    padding: 0.4rem 0.75rem;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    color: var(--primary-text-color);
    font-size: 0.9rem;
    line-height: 1.2;
  }
  a.btn-link:hover {
    background: color-mix(in srgb, var(--primary-text-color) 8%, transparent);
  }

  /* Live input-level meter — same look as the routing-graph meter. */
  .meter {
    flex: 0 1 120px;
    min-width: 48px;
    max-width: 160px;
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
