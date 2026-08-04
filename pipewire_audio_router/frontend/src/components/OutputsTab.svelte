<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { api, MIN_OUTPUT_NAME_CHARS } from '../lib/api';
  import { routing } from '../lib/routing';
  import { run, toast } from '../lib/toast';
  import type { OpResponse, OutputInfo, SendspinCodec } from '../lib/types';
  import AgentsPanel from './AgentsPanel.svelte';
  import GroupTitle from './GroupTitle.svelte';
  import OutputsDocs from './OutputsDocs.svelte';
  import ReceiverAgentDocs from './ReceiverAgentDocs.svelte';
  import VolumeControl from './VolumeControl.svelte';

  // Two listings, because discovery only *offers* a device: `outputs` is what
  // the user has added (routable, exposed to Home Assistant, tunable) and
  // `offered` is everything found but not added — both the undecided ones and
  // the ignored ones, which the checkbox below filters apart client-side.
  // The two help documents behind the header buttons. The page keeps a sentence;
  // the explanations (and the agent downloads) live in the dialogs, the same split
  // the Music groups, Announcements and Sources pages use.
  let outputsDocsOpen = $state(false);
  let agentDocsOpen = $state(false);

  let outputs = $state<OutputInfo[]>([]);
  let offered = $state<OutputInfo[]>([]);
  let showIgnored = $state(false);
  let loading = $state(true);

  const pending = $derived(offered.filter((o) => o.state === 'discovered'));
  const ignored = $derived(offered.filter((o) => o.state === 'ignored'));

  // Per-device volume + mute come from the **live routing matrix** over the
  // WebSocket — the same source the routing graph reads, so the two pages can't
  // disagree. It has to be live: an AirPlay-2 receiver's level is
  // device-authoritative and arrives when it connects (after this tab has
  // rendered), a sendspin speaker reports its own knob turns, and Home Assistant
  // or the graph can change either behind our back. A one-shot fetch on mount
  // showed whatever was true at page load and then quietly went stale.
  //
  // Nothing extra is paid for the subscription: App.svelte holds the same store
  // open for the header's connection dot, so the socket is up for as long as the
  // UI is. `null` = genuinely unknown (never fabricate full scale), and
  // <VolumeControl> owns the drag guard that keeps an incoming frame from yanking
  // the thumb mid-drag.
  const liveVol = $derived(
    new Map($routing.matrix.outputs.map((o) => [o.node_name, o.volume == null ? null : Math.round(o.volume * 100)])),
  );

  // Mute is mirrored into local state rather than read straight through, so the
  // toggle can be optimistic (the matrix confirms a frame later). `untrack` keeps
  // this effect from depending on its own writes.
  let muted = $state<Record<string, boolean>>({});
  $effect(() => {
    const outs = $routing.matrix.outputs;
    untrack(() => {
      let next = muted;
      let changed = false;
      for (const o of outs) {
        if (typeof o.muted === 'boolean' && next[o.node_name] !== o.muted) {
          if (!changed) next = { ...muted };
          next[o.node_name] = o.muted;
          changed = true;
        }
      }
      if (changed) muted = next;
    });
  });

  // Per-device sync tuning: static delays (sendspin) and per-output render delay
  // (AirPlay 2). The daemon-wide group lead lives on the Settings tab.
  // Per-row editable sync value (ms): AP2 render delay or sendspin static delay.
  let edit = $state<Record<string, number | ''>>({});

  // AP2 render-delay slider. The daemon's default when no override is stored
  // (ap2_server.rs AP2_RENDER_DELAY_MS) — the slider has to show *something*, so
  // an output with no override sits at the value it is actually running.
  const AP2_DELAY_DEFAULT = 1500;
  const AP2_DELAY_MAX = 2000;
  // Below this the receiver has too little buffer to absorb send-side jitter, so
  // packets can arrive past their play deadline and get dropped — audible as
  // dropouts or total silence. Allowed (finding your hardware's floor is the
  // point of the knob), but flagged.
  const AP2_DELAY_RISKY_BELOW = 200;
  // Above this it plays fine but you're buying latency you probably don't need.
  const AP2_DELAY_HIGH_ABOVE = 800;

  type DelayZone = 'risky' | 'good' | 'high';
  function delayZone(ms: number): DelayZone {
    if (ms < AP2_DELAY_RISKY_BELOW) return 'risky';
    if (ms > AP2_DELAY_HIGH_ABOVE) return 'high';
    return 'good';
  }
  function delayNote(o: OutputInfo, ms: number): string {
    const origin = o.latency_ms == null ? `default ${AP2_DELAY_DEFAULT} ms` : 'your override';
    if (ms < AP2_DELAY_RISKY_BELOW)
      return `Below ${AP2_DELAY_RISKY_BELOW} ms the receiver may drop late packets — expect dropouts or silence on a jittery sender (${origin}).`;
    if (ms > AP2_DELAY_HIGH_ABOVE)
      return `Safe, but ${ms} ms of buffering is added latency you may not need (${origin}).`;
    return `In the range that buffers enough without adding needless latency (${origin}).`;
  }
  // Slider position as a number even before anything is typed/stored.
  const delayOf = (o: OutputInfo) => {
    const v = edit[o.node_name];
    return typeof v === 'number' ? v : (o.latency_ms ?? AP2_DELAY_DEFAULT);
  };

  // Drop the override so the output follows the daemon default again. The slider
  // has no "empty" position, so clearing needs its own control.
  async function resetDelay(o: OutputInfo) {
    if (await run(() => api.setOutputLatency(o.node_name, null), `Reset '${o.name}' render delay to default`))
      await refresh();
  }

  async function refresh() {
    loading = true;
    try {
      const [outs, disc, delays] = await Promise.all([
        api.outputs(),
        api.discoveredOutputs().catch(() => [] as OutputInfo[]),
        api.sendspinDelays().catch(() => ({}) as Record<string, number>),
      ]);
      outputs = outs;
      // Present devices first — a long discovered list on a busy network is
      // mostly things that are reachable *now*, and those are the ones you can
      // identify with a test tone.
      offered = disc.sort((a, b) => Number(b.present) - Number(a.present) || a.name.localeCompare(b.name));
      // Seed the editable sync fields. Adopted outputs only: the sync knobs are
      // for outputs that are part of the system, not for a device we're still
      // deciding about. Volume/mute are NOT seeded here — they ride the live
      // matrix (above), which is why they stay correct after page load.
      const next: Record<string, number | ''> = {};
      for (const o of outs) {
        // AP2 seeds to the running value rather than blank: its control is a
        // slider, which has no blank position (see resetDelay for clearing).
        next[o.node_name] =
          o.kind === 'sendspin' ? (delays[o.node_name] ?? 0) : (o.latency_ms ?? AP2_DELAY_DEFAULT);
      }
      edit = next;
    } catch {
      outputs = [];
      offered = [];
    }
    loading = false;
  }
  onMount(refresh);

  // Per-row diagnostic playback ("Play tone" / "Play announcement") via the
  // per-device announce path (`/api/announce`) — the backend-agnostic route
  // that ducks + overlays a clip on one device. Keyed by node name so only the
  // pressed row's buttons show the busy state.
  let testing = $state<Record<string, 'tone' | 'announce' | null>>({});

  // Add / ignore / remove in flight, keyed by node name — these re-fetch both
  // listings, so the row's buttons stay disabled until it settles.
  let deciding = $state<Record<string, boolean>>({});

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
    if (o.kind === 'sendspin') return 'Sendspin';
    if (o.kind === 'pwsink') return 'PipeWire';
    return 'AirPlay 2';
  }

  // Status badge, three-state: offline (not reachable) / not connected (no session) /
  // online. `present` is reachability only — mDNS presence as owned by the liveness
  // tasks. For pw-sink that really is all it means: the remote host advertises over
  // mDNS, but audio only flows once its module-rtp-session initiates the AppleMIDI
  // handshake (receiver-driven), so reachable-but-unattached is a genuine third
  // state. Folding it into "offline" (as this page used to) is what made it
  // disagree with the routing graph about the same target — the graph read
  // presence, this page read the session. Now both show all three states.
  //
  // Only an *added* pw-sink target has a session to wait for; a merely discovered
  // one has none by definition, so it just reads online.
  function statusBadge(o: OutputInfo): { cls: string; text: string; title: string } {
    if (!o.present) return { cls: 'badge off', text: 'offline', title: 'Not on the network right now. Its routing is kept and reapplied when it returns.' };
    if (o.kind === 'pwsink' && o.state === 'adopted' && o.pwsink_streaming !== true) {
      return {
        cls: 'badge caution',
        text: 'not connected',
        title:
          "On the network, but no receiver has connected to the session we advertise — its module-rtp-session initiates the handshake, so until it does, anything routed here isn't played. Announcements open a temporary session instead.",
      };
    }
    return { cls: 'badge on', text: 'online', title: 'On the network and carrying audio routed to it.' };
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
      return { cls: 'badge on', text: 'PTP ✓', title: `Exchanging PTP with our clock${age} — multi-room sync is tight.` };
    }
    if (o.ptp_supported === false) {
      return {
        cls: 'badge',
        text: 'PTP n/a',
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
      text: 'PTP —',
      title: `Not exchanging PTP with us${age} — fine for single-room realtime playback; a live PTP lock only matters for keeping multiple rooms in sync.`,
    };
  }

  // Which outputs can be announced to individually via the per-device path
  // (/api/announce). Every output kind is a per-device sender wired into the
  // OverlayMixer: sendspin, AirPlay 2 (own overlay path), and pw-sink (the
  // per-target AppleMIDI relay applies mix_into). Neither dialed backend needs a
  // wired input — the daemon opens an on-demand session for an unrouted AirPlay-2
  // receiver or pw-sink target — so the gate is reachability (`present`), NOT
  // whether a session is attached: standing one up is exactly what this does.
  //
  // For AirPlay 2 and pw-sink this deliberately works on a merely *discovered*
  // device too: playing a tone is how you find out which speaker
  // `ap2-dev-living-2` is before adding it.
  //
  // Sendspin is the exception. A sendspin speaker is only reachable while the
  // daemon holds a WebSocket to it, and it doesn't open one to a device that
  // hasn't been added — and there's no on-demand equivalent, because a fresh
  // sendspin connection takes tens of seconds to start rendering (ESPHome
  // firmware), so a "test tone" over it would land long after you'd stopped
  // listening. Add it first; then it's instantly testable like anything else.
  function canTest(o: OutputInfo): boolean {
    if (!o.present) return false;
    if (o.kind === 'sendspin') return o.state === 'adopted';
    return o.kind === 'airplay2' || o.kind === 'pwsink';
  }
  function testHint(o: OutputInfo): string {
    if (!o.present) return 'Output is offline';
    if (o.kind === 'sendspin' && o.state !== 'adopted')
      return 'Add this speaker first — the router only connects to a Sendspin device once it has been added, and a fresh connection takes tens of seconds to start playing, so there is no instant test tone before that';
    // No session attached yet (`pwsink_streaming`), so the clip rides an on-demand
    // one — not `isOnline`, which is only reachability.
    if (o.kind === 'pwsink' && o.pwsink_streaming !== true)
      return 'Opens a temporary session — the target connects to it, so audio starts a moment later';
    return '';
  }

  // The daemon's own message is the honest one: whether the clip is playing now,
  // queued behind another, or waiting on an on-demand AirPlay session that takes a
  // few seconds to pair — so surface it verbatim instead of a fixed "Played …".
  async function playClip(o: OutputInfo, what: 'tone' | 'announce') {
    testing = { ...testing, [o.node_name]: what };
    try {
      const res = await api.announce({ targets: [o.node_name], ...(what === 'tone' ? { tone: true } : { test: true }) });
      toast(res.ok ? 'success' : 'error', res.message);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
    testing = { ...testing, [o.node_name]: null };
  }
  const playTone = (o: OutputInfo) => playClip(o, 'tone');
  const playAnnouncement = (o: OutputInfo) => playClip(o, 'announce');

  // Resync: ask a sendspin device to drop its buffered audio and re-anchor
  // (`stream/clear`). For the failure mode where a device is demonstrably being
  // *sent* audio and plays none — measured on 2026-08-03, when three of four went
  // silent while the daemon, the graph and the clock sync were all healthy. Before
  // this the only lever was restarting the add-on, which interrupted every other
  // output. Only offered for a connected sendspin device: there is nothing to clear
  // otherwise, and the daemon says so rather than pretending.
  let clearing = $state<Record<string, boolean>>({});
  const canClear = (o: OutputInfo) => o.kind === 'sendspin' && o.present;
  async function resync(o: OutputInfo) {
    clearing = { ...clearing, [o.node_name]: true };
    try {
      const res = await api.sendspinClear(o.node_name);
      toast(res.ok ? 'success' : 'error', res.message);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
    clearing = { ...clearing, [o.node_name]: false };
  }

  // Per-device volume / mute for present sendspin + AirPlay-2 outputs, from the
  // always-visible card header. No local echo of the new level: the daemon pushes
  // a matrix frame back, and <VolumeControl>'s drag guard holds the thumb until
  // it lands — so the value on screen is always one the daemon confirmed.
  async function onVolume(o: OutputInfo, pct: number) {
    try {
      if (o.kind === 'airplay2') await api.setAp2Volume(o.node_name, pct / 100);
      else await api.setSendspinVolume(o.node_name, pct);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
  async function onMute(o: OutputInfo) {
    const next = !muted[o.node_name];
    muted = { ...muted, [o.node_name]: next }; // optimistic; the matrix confirms
    try {
      if (o.kind === 'airplay2') await api.setAp2Mute(o.node_name, next);
      else await api.setSendspinMute(o.node_name, next);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }

  // Add / ignore / remove. All three move a device between the two listings, so
  // each re-fetches both — and each surfaces the daemon's own message, which says
  // what else it touched ("also cleared its routing and group membership").
  // `okText` overrides the daemon's message for the one action whose wording
  // wouldn't fit: un-ignoring is the same "back to undecided" call as removing,
  // and "removed …" would read as the opposite of what the user just clicked.
  async function decide(o: OutputInfo, call: () => Promise<OpResponse>, okText?: string) {
    deciding = { ...deciding, [o.node_name]: true };
    try {
      const res = await call();
      toast(res.ok ? 'success' : 'error', res.ok ? (okText ?? res.message) : res.message);
      if (res.ok) await refresh();
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
    deciding = { ...deciding, [o.node_name]: false };
  }
  const add = (o: OutputInfo) => decide(o, () => api.adoptOutput(o.node_name));
  const ignore = (o: OutputInfo) => decide(o, () => api.ignoreOutput(o.node_name));
  const unignore = (o: OutputInfo) => decide(o, () => api.removeOutput(o.node_name), `'${o.name}' is back in the discovered list`);

  // Removing is destructive of configuration (routing links, group membership,
  // the HA media_player), and the daemon can't undo it, so ask first. A device
  // that's still on the network comes back as a discovered offer.
  function remove(o: OutputInfo) {
    const back = o.present ? `'${o.name}' stays on the network and will reappear below as a discovered device.` : `'${o.name}' is offline, so it will disappear from this page until it shows up again.`;
    if (!confirm(`Remove '${o.name}' from your outputs?\n\nIts routing, group membership and Home Assistant media_player are removed. ${back}`)) return;
    return decide(o, () => api.removeOutput(o.node_name));
  }

  // Rename an output. The daemon stores the name against the output's stable node
  // name, so it survives the device dropping off the network and re-resolving —
  // and it is the name every other page (and Home Assistant) then shows. Nothing
  // restarts; the refresh is only so this card stops showing the old name.
  async function rename(o: OutputInfo, name: string) {
    if (await run(() => api.renameOutput(o.node_name, name), `Renamed to '${name}'`)) await refresh();
  }

  // Drop the rename: the output goes back to the name its device announces. Only
  // offered for an output that actually carries one (`renamed`) — the discovered
  // name isn't in this listing, so there's nothing to preview and no point
  // offering the control otherwise. Routed through `decide` for its toast of the
  // daemon's own message, which is what names the result.
  const resetName = (o: OutputInfo) => decide(o, () => api.renameOutput(o.node_name, null));

  // AirPlay-2 wire sample-rate mode (auto vs forced 44.1 kHz). Restarts the group.
  async function setRateMode(o: OutputInfo, e: Event) {
    const mode = (e.currentTarget as HTMLSelectElement).value as 'auto' | 'fixed_44100';
    await run(() => api.setAp2RateMode(o.node_name, mode),
      `Set '${o.name}' sample rate to ${mode === 'auto' ? 'auto' : '44.1 kHz'}`);
    await refresh();
  }

  // Sendspin wire codec. Restarts that device's group (one stream = one format), so
  // the daemon sends a fresh stream/start; unavailable options are disabled below, so
  // this only ever posts something the daemon accepts.
  //
  // A codec change also moves this speaker's *buffer requirement* — a compressed codec
  // needs a head start for decode warmup, and the daemon raises the group's send-ahead
  // to cover it. That value only arrives once the device has reconnected and reported
  // it, which takes a moment, so refresh again after the restart has settled instead of
  // leaving a stale figure on screen.
  async function setCodec(o: OutputInfo, e: Event) {
    const codec = (e.currentTarget as HTMLSelectElement).value as SendspinCodec;
    await run(() => api.setSendspinCodec(o.node_name, codec), `Set '${o.name}' codec to ${codec}`);
    await refresh();
    setTimeout(refresh, 3000);
  }

  // What this speaker asked us to keep buffered, and what its stream actually gets.
  // Both are protocol-driven, not preferences: the daemon must not send less than the
  // device's `min_buffer_ms`, so this is where a "it stutters" investigation starts.
  function bufferNote(o: OutputInfo): string | null {
    if (o.kind !== 'sendspin' || o.sendspin_send_ahead_ms == null) return null;
    if (o.sendspin_min_buffer_ms == null) {
      return `sending ${o.sendspin_send_ahead_ms} ms ahead (this speaker hasn't reported a buffer requirement yet)`;
    }
    const asked = o.sendspin_min_buffer_ms;
    return `asks for ${asked} ms buffer, sending ${o.sendspin_send_ahead_ms} ms ahead`;
  }

  // Label for a codec option, with the bandwidth trade-off spelled out — that's the
  // whole reason to pick one.
  function codecLabel(codec: string): string {
    if (codec === 'auto') return 'Auto (Opus when the speaker supports it, else PCM)';
    if (codec === 'pcm') return 'PCM — uncompressed, ~1.5 Mbit/s';
    if (codec === 'opus') return 'Opus — lossy, ~10× less WiFi traffic';
    if (codec === 'flac') return 'FLAC — lossless, ~2× less WiFi traffic';
    return codec;
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

<!-- Connection details — the same set for an added output and a discovered one,
     because identifying a device ("is that the one in the kitchen?") is exactly
     what you need before deciding whether to add it. -->
{#snippet connMeta(o: OutputInfo)}
  <dl class="out-meta">
    <div><dt>IP</dt><dd>{#if o.ip}<code>{o.ip}</code>{:else}—{/if}</dd></div>
    <div><dt>Port</dt><dd>{o.port ?? '—'}</dd></div>
    <div><dt>Encryption</dt><dd>{o.encryption ?? '—'}</dd></div>
  </dl>
{/snippet}

<!-- Diagnostic playback. Works for a discovered device too: the daemon opens an
     on-demand session for it, which is how you tell which speaker this is. -->
{#snippet playActions(o: OutputInfo)}
  <div class="btn-group out-play" title={testHint(o)}>
    <button class="ghost seg label" disabled>Play:</button>
    <button class="ghost seg" disabled={!canTest(o) || testing[o.node_name] != null} onclick={() => playTone(o)}>
      {testing[o.node_name] === 'tone' ? 'Playing…' : 'Tone'}
    </button>
    <button class="ghost seg" disabled={!canTest(o) || testing[o.node_name] != null} onclick={() => playAnnouncement(o)}>
      {testing[o.node_name] === 'announce' ? 'Playing…' : 'Announcement'}
    </button>
  </div>
  {#if canClear(o)}
    <!-- Recovery, not playback, so it sits outside the Play group. -->
    <button
      class="ghost out-resync"
      disabled={clearing[o.node_name]}
      title="Discard this speaker's buffered audio and re-anchor it (stream/clear). Try this if it is connected but silent — it does not disturb the other speakers in its group."
      onclick={() => resync(o)}
    >{clearing[o.node_name] ? 'Resyncing…' : 'Resync'}</button>
  {/if}
{/snippet}

{#snippet codecField(o: OutputInfo)}
  {#if o.kind === 'sendspin' && o.sendspin_codec_options}
    <div class="sync-field">
      <label for="codec-{o.node_name}">Codec</label>
      <div class="sync-cell">
        <select
          id="codec-{o.node_name}"
          value={o.sendspin_codec ?? 'auto'}
          onchange={(e) => setCodec(o, e)}
          title="Wire format for this speaker's audio. Options the add-on can't encode, or that this device didn't advertise, are unavailable."
        >
          {#each o.sendspin_codec_options as opt (opt.codec)}
            <option value={opt.codec} disabled={!opt.available} title={opt.reason ?? ''}>
              {codecLabel(opt.codec)}{opt.available ? '' : ' — unavailable'}
            </option>
          {/each}
        </select>
        <!-- What it actually resolves to, which differs from the choice
             whenever the pick isn't usable. -->
        <span class="muted" style="white-space:nowrap">
          {o.sendspin_codec_active ? `using ${o.sendspin_codec_active}` : ''}
        </span>
      </div>
      {#if bufferNote(o)}
        <!-- Moves with the codec, which is why changing it refreshes this. -->
        <p class="muted" style="font-size:0.8rem; margin:4px 0 0">{bufferNote(o)}</p>
      {/if}
    </div>
  {/if}
{/snippet}

<div class="card info">
  <div class="info-head">
    <h2>Supported outputs</h2>
    <div class="info-actions">
      <button
        class="ghost"
        type="button"
        title="The kinds of output, what discovered vs added means, and what each state tells you"
        onclick={() => (outputsDocsOpen = true)}
      >
        Explain outputs
      </button>
      <button
        class="ghost"
        type="button"
        title="Turn a Linux machine into an output: download the agent, install it, pair it"
        onclick={() => (agentDocsOpen = true)}
      >
        Explain receiver hosts
      </button>
    </div>
  </div>
  <p class="card-sub" style="margin-bottom:0">
    <strong>AirPlay 2</strong> receivers, <strong>Sendspin</strong> speakers, and Linux machines running
    the receiver agent. Devices on your network are found automatically but only <em>offered</em> — one
    does nothing until you <strong>add</strong> it.
  </p>
</div>

{#if loading}
  <div class="card"><p class="empty" style="padding:0">Loading…</p></div>
{:else}
  <div class="section-head">
    <h3>
      Your outputs
      {#if outputs.length}<span class="count">{outputs.length}</span>{/if}
    </h3>
  </div>

  {#if outputs.length === 0}
    <div class="card">
      <p class="empty" style="padding:0">
        No outputs yet — add one from the discovered devices below. Until you do, nothing is routable and no
        Home Assistant media players are created.
      </p>
    </div>
  {:else}
    {#each outputs as o (o.node_name)}
      {@const st = statusBadge(o)}
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
            <!-- Renamed in place, the same control the group cards use: a
                 device's mDNS name is often no help in a house (four speakers
                 all called "Yamaha"), and this name is what the routing graph,
                 the group chips and Home Assistant show. The clear icon appears
                 only once a name of your own is stored, and asks first — the
                 device's own name isn't shown anywhere to type back. -->
            <h3>
              <GroupTitle
                name={o.name}
                minLength={MIN_OUTPUT_NAME_CHARS}
                title="Rename this output"
                onRename={(name) => rename(o, name)}
                onReset={o.renamed ? () => resetName(o) : undefined}
                resetTitle="Use the name this device announces"
                resetConfirm={`Drop the name '${o.name}' and use the one this device announces?`}
              />
            </h3>
          </div>
          <!-- Badges sit at the right, in the order that keeps their columns
               aligned down the list: the two every output has (type, then
               status) are anchored against the volume control, and the optional
               PTP badge hangs off to their left rather than displacing them. -->
          <div class="out-badges">
            {#if ptpBadge(o)}
              {@const ptp = ptpBadge(o)!}
              <span class={ptp.cls} title={ptp.title}>{ptp.text}</span>
            {/if}
            <span class={st.cls} title={st.title}>{st.text}</span>
            <span class="badge">{kindLabel(o)}</span>
          </div>
          <!-- The volume column is kept even for an output that has no volume to
               control (pw-sink, or anything offline), so the badges beside it line
               up with every other row's instead of sliding right. -->
          <div class="out-vol">
            {#if o.present && (o.kind === 'airplay2' || o.kind === 'sendspin')}
              <VolumeControl
                percent={liveVol.get(o.node_name) ?? null}
                muted={muted[o.node_name] ?? false}
                onVolume={(pct) => onVolume(o, pct)}
                onMute={() => onMute(o)}
              />
            {/if}
          </div>
        </header>

        {#if !isCollapsed(o)}
          {@render connMeta(o)}

          <!-- Guarded: with the actions moved to their own row a pw-sink output has
               no tuning knobs left, and an empty bordered row reads as broken. -->
          {#if o.kind === 'sendspin' || o.kind === 'airplay2'}
          <div class="out-controls">
            {#if o.kind === 'sendspin'}
            <div class="sync-field">
              <label for="sync-{o.node_name}">Static delay (ms)</label>
              <div class="sync-cell">
                <input
                  id="sync-{o.node_name}"
                  type="number"
                  min="0"
                  max="5000"
                  step="10"
                  bind:value={edit[o.node_name]}
                  placeholder="0"
                  title="Static delay in ms (0 = none)"
                />
                <button onclick={() => applySync(o)} title="Apply">Set</button>
              </div>
            </div>
            {/if}

            {#if o.kind === 'airplay2'}
              {@const ms = delayOf(o)}
              {@const zone = delayZone(ms)}
              <!-- A slider rather than a number box: this is a knob you hunt for
                   the edge of by ear, and the coloured track shows which way is
                   risky before you drag there. -->
              <div class="sync-field delay-field">
                <label for="sync-{o.node_name}">Render delay</label>
                <div class="delay-cell">
                  <!-- Slider and scale share one box so the tick numbers land on
                       the colour boundaries — measured against the whole field
                       they drift right by the readout's width. -->
                  <div class="delay-track">
                    <input
                      id="sync-{o.node_name}"
                      class="delay-slider zone-{zone}"
                      type="range"
                      min="0"
                      max={AP2_DELAY_MAX}
                      step="10"
                      value={ms}
                      aria-describedby="delay-note-{o.node_name}"
                      oninput={(e) => (edit[o.node_name] = Number(e.currentTarget.value))}
                      onchange={() => applySync(o)}
                    />
                    <div class="delay-scale" aria-hidden="true">
                      <span style="left:0%">0</span>
                      <span style="left:{(AP2_DELAY_RISKY_BELOW / AP2_DELAY_MAX) * 100}%">{AP2_DELAY_RISKY_BELOW}</span>
                      <span style="left:{(AP2_DELAY_HIGH_ABOVE / AP2_DELAY_MAX) * 100}%">{AP2_DELAY_HIGH_ABOVE}</span>
                      <span style="left:100%">{AP2_DELAY_MAX}</span>
                    </div>
                  </div>
                  <output class="delay-read zone-{zone}" for="sync-{o.node_name}">{ms} ms</output>
                  <button
                    class="ghost"
                    disabled={o.latency_ms == null}
                    title="Drop the override and follow the add-on default ({AP2_DELAY_DEFAULT} ms)"
                    onclick={() => resetDelay(o)}
                  >Default</button>
                </div>
                <p id="delay-note-{o.node_name}" class="delay-note zone-{zone}">{delayNote(o, ms)}</p>
              </div>
            {/if}

            {@render codecField(o)}

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

          <!-- Actions get their own row, away from the knobs: the delay slider is
               wide enough that sharing a row crowded the buttons, and it keeps a
               destructive Remove from sitting next to controls you drag. -->
          <div class="out-actions">
            {@render playActions(o)}
            <div class="out-remove">
              <button class="danger" disabled={deciding[o.node_name]} onclick={() => remove(o)}>
                {deciding[o.node_name] ? 'Removing…' : 'Remove'}
              </button>
            </div>
          </div>
        {/if}
      </article>
    {/each}
  {/if}

  <!-- Receiver hosts (pwrouter-agent). Above Discovered on purpose: a remote
       PipeWire host cannot appear as a discovered device until its agent is
       paired, so this is the earlier step in the same flow. Refreshing both
       listings on a decision keeps the two sections consistent without a
       page reload. -->
  <AgentsPanel onchange={refresh} />

  <div class="section-head">
    <h3>
      Discovered devices
      {#if pending.length}<span class="count">{pending.length}</span>{/if}
    </h3>
    {#if ignored.length}
      <label class="check">
        <input type="checkbox" bind:checked={showIgnored} />
        Show ignored ({ignored.length})
      </label>
    {/if}
  </div>

  {#if pending.length === 0 && (!showIgnored || ignored.length === 0)}
    <div class="card">
      <p class="empty" style="padding:0">
        {#if ignored.length}
          Nothing new — {ignored.length} discovered {ignored.length === 1 ? 'device is' : 'devices are'} ignored.
        {:else}
          Nothing new. Compatible AirPlay 2 / Sendspin / PipeWire devices appear here automatically when
          they're on the network.
        {/if}
      </p>
    </div>
  {/if}

  {#each showIgnored ? [...pending, ...ignored] : pending as o (o.node_name)}
    {@const st = statusBadge(o)}
    <article class="card out-card offer" class:offline={!o.present} class:collapsed={isCollapsed(o)} class:dismissed={o.state === 'ignored'}>
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
            <span class={st.cls} title={st.title}>{st.text}</span>
            {#if o.state === 'ignored'}
              <span class="badge" title="Ignored — hidden unless 'Show ignored' is ticked. Adding it clears that.">ignored</span>
            {/if}
          </div>
        </div>
        <!-- The decision itself is one click from the collapsed row: you
             shouldn't have to expand a card to dismiss a neighbour's speaker. -->
        <div class="out-decide">
          <button class="primary" disabled={deciding[o.node_name]} onclick={() => add(o)}>
            {deciding[o.node_name] ? 'Working…' : 'Add'}
          </button>
          {#if o.state === 'ignored'}
            <button class="ghost" disabled={deciding[o.node_name]} title="Move back to the discovered list" onclick={() => unignore(o)}>
              Un-ignore
            </button>
          {:else}
            <button class="ghost" disabled={deciding[o.node_name]} title="Hide this device from the discovered list" onclick={() => ignore(o)}>
              Ignore
            </button>
          {/if}
        </div>
      </header>

      {#if !isCollapsed(o)}
        {@render connMeta(o)}
        <div class="out-controls">
          {@render playActions(o)}
          {@render codecField(o)}
        </div>
        <p class="muted offer-note">
          Not routable and not in Home Assistant until you add it. Delay tuning and volume become available then.
        </p>
      {/if}
    </article>
  {/each}
{/if}

{#if outputsDocsOpen}
  <OutputsDocs onClose={() => (outputsDocsOpen = false)} />
{/if}

{#if agentDocsOpen}
  <ReceiverAgentDocs onClose={() => (agentDocsOpen = false)} />
{/if}

<style>
  /* Card header with help buttons — same shape as the Input-sources card on the
     Sources page, so the two pages read alike. */
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

  /* ---- Section headings between the two listings ------------------------- */
  .section-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
    margin: 22px 4px 10px;
  }
  .section-head h3 {
    margin: 0;
    font-size: 0.8rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--secondary-text-color);
  }
  .count {
    display: inline-block;
    margin-left: 6px;
    padding: 1px 7px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--primary-color) 16%, transparent);
    color: var(--primary-color);
    font-size: 0.72rem;
    letter-spacing: 0;
  }
  .check {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin: 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    cursor: pointer;
  }
  .check input {
    width: auto;
    margin: 0;
    accent-color: var(--primary-color);
  }

  /* ---- One card per output ---------------------------------------------- */
  .out-card.offline {
    opacity: 0.6;
  }
  /* An offered device is visually quieter than an adopted one — it isn't part
     of the system yet — and an ignored one quieter still. */
  .out-card.offer {
    border-style: dashed;
  }
  .out-card.dismissed {
    opacity: 0.55;
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
  /* Always laid out, empty or not — it is the column the badges are aligned
     against (see the header markup). */
  .out-vol {
    flex: 0 0 auto;
    width: 160px;
    max-width: 40%;
  }
  .out-decide {
    flex: 0 0 auto;
    display: flex;
    gap: 8px;
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
    min-width: 0;
  }
  /* <GroupTitle> brings the group cards' sizing with it (`.gtitle` in app.css);
     inside this heading it should read as the heading, editing or not. The
     padding it needs for its hover target is pulled back off the left so the
     name still lines up with the badges under it. */
  .out-title h3 :global(.gtitle) {
    margin-left: -4px;
  }
  .out-title h3 :global(.gtitle strong) {
    font-size: inherit;
    font-weight: inherit;
  }
  .out-title h3 :global(.rename) {
    font-size: inherit;
    font-weight: inherit;
  }
  .out-badges {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
    /* In an adopted card's header this is a column of its own between the name
       and the volume control: it keeps its content width but may shrink and wrap
       internally on a narrow card, staying right-aligned as it does. */
    flex: 0 1 auto;
    justify-content: flex-end;
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

  /* Tuning knobs, in one left-aligned row: the delay control, then any
     kind-specific field. Actions live in .out-actions below.
     Top-aligned, not bottom: the codec field carries a note under its select, so
     bottom-alignment lifted its label clear of the static-delay label beside it.
     Every field here is label-then-control with a one-line label, so aligning
     the tops lines the input boxes up as well, and a note just hangs below. */
  .out-controls {
    display: flex;
    align-items: flex-start;
    gap: 16px;
    flex-wrap: wrap;
    padding-top: 14px;
    border-top: 1px solid var(--divider-color);
  }
  /* Actions row: play/resync at the start, Remove pushed to the far end. */
  .out-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
    margin-top: 14px;
    padding-top: 14px;
    border-top: 1px solid var(--divider-color);
  }
  .out-play {
    flex: 0 0 auto;
  }

  /* ---- AP2 render-delay slider ------------------------------------------
     A custom range whose track is painted with the three zones, so which way
     is risky is visible before you drag: red below 200 ms (too little buffer
     to absorb sender jitter → dropped packets), green through 800 ms, yellow
     above it (fine, just needless latency). Hard stops rather than a gradient
     — the thresholds are the information. */
  /* Its own full-width row: the track plus a readout, a reset and a note is more
     than fits beside the sample-rate picker, which wraps below instead. */
  .delay-field {
    flex: 1 1 100%;
  }
  .delay-cell {
    display: flex;
    gap: 10px;
    align-items: center;
  }
  .delay-track {
    flex: 1 1 auto;
    min-width: 160px;
  }
  .delay-slider {
    display: block;
    width: 100%;
    /* Own the whole control: default chrome ignores a painted track. */
    appearance: none;
    -webkit-appearance: none;
    height: 18px;
    margin: 0;
    padding: 0;
    background: transparent;
    cursor: pointer;
  }
  /* 200/2000 = 10%, 800/2000 = 40%. The two vendor pseudo-elements must be
     separate rules — a browser drops the whole rule if any selector in the list
     is one it doesn't know. */
  .delay-slider::-webkit-slider-runnable-track {
    height: 6px;
    border-radius: 3px;
    background: linear-gradient(
      to right,
      var(--error-color) 0 10%,
      var(--success-color) 10% 40%,
      var(--warning-color) 40% 100%
    );
  }
  .delay-slider::-moz-range-track {
    height: 6px;
    border-radius: 3px;
    background: linear-gradient(
      to right,
      var(--error-color) 0 10%,
      var(--success-color) 10% 40%,
      var(--warning-color) 40% 100%
    );
  }
  .delay-slider::-webkit-slider-thumb {
    appearance: none;
    -webkit-appearance: none;
    width: 16px;
    height: 16px;
    margin-top: -5px; /* centre on the 6px track */
    border-radius: 50%;
    border: 2px solid var(--card-background-color, #fff);
    background: var(--primary-text-color);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.35);
  }
  .delay-slider::-moz-range-thumb {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    border: 2px solid var(--card-background-color, #fff);
    background: var(--primary-text-color);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.35);
  }
  .delay-slider:focus-visible {
    outline: 2px solid var(--primary-color);
    outline-offset: 3px;
    border-radius: 3px;
  }
  /* The thumb takes the zone colour too, so the current value is readable
     without comparing it against the track by eye. */
  .delay-slider.zone-risky::-webkit-slider-thumb {
    background: var(--error-color);
  }
  .delay-slider.zone-risky::-moz-range-thumb {
    background: var(--error-color);
  }
  .delay-slider.zone-high::-webkit-slider-thumb {
    background: var(--warning-color);
  }
  .delay-slider.zone-high::-moz-range-thumb {
    background: var(--warning-color);
  }
  .delay-slider.zone-good::-webkit-slider-thumb {
    background: var(--success-color);
  }
  .delay-slider.zone-good::-moz-range-thumb {
    background: var(--success-color);
  }

  .delay-read {
    min-width: 68px; /* "2000 ms" without the row twitching as you drag */
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-size: 0.9rem;
    font-weight: 500;
  }
  .delay-read.zone-risky {
    color: var(--error-color);
  }
  .delay-read.zone-high {
    color: var(--warning-color);
  }

  /* Zone-edge numbers under the track, positioned at their value's fraction. */
  .delay-scale {
    position: relative;
    height: 1em;
    margin: 2px 0 0;
    font-size: 0.7rem;
    color: var(--secondary-text-color);
    font-variant-numeric: tabular-nums;
  }
  .delay-scale span {
    position: absolute;
    transform: translateX(-50%);
  }
  /* The end labels align flush with the track's ends instead of straddling them,
     so they don't hang out into the value readout. */
  .delay-scale span:first-child {
    transform: none;
  }
  .delay-scale span:last-child {
    transform: translateX(-100%);
  }

  .delay-note {
    font-size: 0.8rem;
    margin: 4px 0 0;
    color: var(--secondary-text-color);
  }
  .delay-note.zone-risky {
    color: var(--error-color);
  }
  /* Recovery, not playback: kept out of the Play segmented group so it doesn't read
     as a third thing to play, but adjacent to it because it answers the question the
     Play buttons raise ("it says playing and I hear nothing"). */
  .out-resync {
    flex: 0 0 auto;
  }
  /* Pushed to the end of the row so a destructive action isn't adjacent to the
     knobs you're clicking through. */
  .out-remove {
    margin-left: auto;
  }
  .offer-note {
    font-size: 0.8rem;
    margin: 12px 0 0;
  }
  .sync-field label {
    display: block;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    margin-bottom: 4px;
  }
  /* A flex item's default min-width is its content, so the long codec <select>
     pushed itself past the card edge once the row got narrow. Letting the field
     and the select shrink fixes that without wrapping the cell — wrapping put
     "using <codec>" on its own line at every width and made the card taller. */
  .out-controls > * {
    min-width: 0;
  }
  .sync-cell {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .sync-cell select {
    min-width: 0;
    max-width: 100%;
  }
  .sync-cell input {
    width: 96px;
  }
</style>
