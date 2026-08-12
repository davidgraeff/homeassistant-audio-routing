<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import { api, MIN_OUTPUT_NAME_CHARS } from '../lib/api';
  import { routing } from '../lib/routing';
  import { run, runUndoable, toast } from '../lib/toast';
  import { askConfirm, removeOutputConfirm } from '../lib/confirm.svelte';
  import type { OpResponse, OutputInfo, SendspinCodec } from '../lib/types';
  import GroupTitle from './GroupTitle.svelte';
  import OutputsDocs from './OutputsDocs.svelte';
  import ReceiverAgentDocs from './ReceiverAgentDocs.svelte';
  import AlignWizard from './AlignWizard.svelte';
  import DelaySlider from './DelaySlider.svelte';
  import VolumeControl from './VolumeControl.svelte';
  import { measure } from '../lib/measure.svelte';

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

  /** Does the daemon have a level it can drive for this output?
   *
   *  `volume`/`muted` are reported exactly when it can (sendspin and AP2 in-band,
   *  pw-sink through the receiver agent) and are null when it cannot — an agent-less
   *  host, or a sink with neither a device route nor node volume. Either is enough:
   *  a member whose level was never reported can still be *set*, which is why
   *  <VolumeControl> keeps the slider live and only changes its tooltip. Deliberately
   *  kind-agnostic, so the next output kind gets its control the moment the daemon
   *  reports state instead of needing this file edited. */
  const levelControl = $derived(
    new Set($routing.matrix.outputs.filter((o) => o.volume != null || o.muted != null).map((o) => o.node_name)),
  );
  const hasLevelControl = (nodeName: string) => levelControl.has(nodeName);

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


  // One delay slider, three kinds. AirPlay 2 shifts its render delay, a PipeWire host
  // sets its receiver's jitter buffer, a sendspin speaker takes a static trim against
  // its group — different mechanisms, one gesture: "this speaker is out of step, move it
  // in time". The scales differ by an order of magnitude, so each kind brings its own
  // spec.
  //
  // Defaults are not hardcoded here: the daemon reports what each output is actually
  // running, as `latency_effective_ms`.
  type DelaySpec = {
    min: number;
    max: number;
    step: number;
    /** Below this the low end is dangerous. **0 disables the risky zone** — for a
     *  knob whose zero simply means "no adjustment", low is not a risk at all. */
    riskyBelow: number;
    highAbove: number;
    label: string;
    /** Sentence for the low end; only used when `riskyBelow > 0`. */
    risk: string;
    /** Sentence for the healthy middle, in this knob's own terms. */
    good: string;
    /** Push intermediate values *while* dragging, so the knob can be found by ear?
     *  Only where applying one is cheap and gapless. */
    liveDuringDrag: boolean;
    /** Is there an *override* to drop (→ a "Default" button)? False for a knob whose
     *  neutral position is 0, which the slider can already reach. */
    clearable: boolean;
    /** Which endpoint holds this value: the shared per-output latency, or sendspin's
     *  own static-delay store. */
    store: 'latency' | 'sendspin';
  };
  const AP2_DELAY: DelaySpec = {
    min: 0,
    max: 2000,
    step: 10,
    // The daemon shifts the next PT=87 anchor on the running stream: no reconnect and
    // nothing to reload, so hearing the drag is worth the throttled round trips.
    liveDuringDrag: true,
    // 0 is the normal position — the receivers render from the anchors as they arrive —
    // so the low end carries no warning. A receiver that goes *silent* needs more delay
    // rather than less, which its fault badge reports; it is not a slider zone.
    riskyBelow: 0,
    // Above this it plays fine, but the delay is latency the whole system pays for.
    highAbove: 200,
    label: 'Render delay',
    risk: '',
    good: 'Shifts when this receiver renders, relative to the rest of its group',
    clearable: true,
    store: 'latency',
  };
  const PWSINK_DELAY: DelaySpec = {
    // Three packet times (sync_settings::PWSINK_JITTER_MIN_MS): the receiving module
    // refuses a buffer below its packet time, and the sender needs room for a
    // catch-up burst *inside* the buffer. 0 is not on offer here the way it is for
    // AP2 — the daemon would clamp it anyway.
    min: 15,
    // Well short of what the API accepts (2000 ms), but far enough to line a remote
    // host up against a slow receiver rather than only to tune its own buffer. The
    // zone marks below stay where they are: past `highAbove` this is still latency
    // you are choosing to add, which is exactly what aligning against something slow
    // means, and the note says so rather than warning you off.
    max: 800,
    step: 5, // a whole number of 5 ms packets, which is what the receiver wants
    // Four packet times: below that the receiver holds under a packet of slack for
    // network or scheduling jitter.
    riskyBelow: 20,
    highAbove: 300,
    label: 'Playout delay',
    risk: 'the receiving host holds under a packet of slack for network or scheduling jitter — expect crackles',
    // Applying one reloads `module-rtp-session` on the remote host — a short gap in
    // that speaker's audio. Doing that at every step of a drag would be a stutter,
    // so this kind commits only when you let go.
    liveDuringDrag: false,
    good: "Buffers the network hop and the remote host's scheduling",
    clearable: true,
    store: 'latency',
  };
  const SENDSPIN_DELAY: DelaySpec = {
    // 0 is the *normal* position here, not a risky one: this knob trims a speaker
    // that is consistently early against the rest of its group, so "no trim" is the
    // resting state and there is no low-end risk zone at all.
    min: 0,
    // The daemon clamps a static delay at 5000 ms, so the slider reaches everything
    // the API accepts. A tighter ceiling would make an already-stored larger value
    // unreachable — and silently shrink it on the next drag.
    max: 5000,
    step: 10,
    riskyBelow: 0,
    highAbove: 1000,
    label: 'Static delay',
    risk: '',
    good: 'Trims this speaker against the rest of its group',
    // The change is in-band, but whether the *firmware* applies it live is a
    // per-setup question (Settings → sendspin delay applied live). Where it does not,
    // every change reconnects that speaker, which costs tens of seconds of silence —
    // so this commits on release, which is right either way.
    liveDuringDrag: false,
    clearable: false,
    store: 'sendspin',
  };
  const delaySpec = (o: OutputInfo) =>
    o.kind === 'pwsink' ? PWSINK_DELAY : o.kind === 'sendspin' ? SENDSPIN_DELAY : AP2_DELAY;
  /** Every adopted kind has one now; still a predicate, so the row keeps its guard. */
  const hasDelayKnob = (o: OutputInfo) => o.kind === 'airplay2' || o.kind === 'pwsink' || o.kind === 'sendspin';

  /** What the daemon has stored for this output right now — the slider's resting
   *  position and its during-drag readout. Sendspin keeps its static delays in a store
   *  of their own (`delayMap`); the rest ride the outputs listing. */
  const appliedMs = (o: OutputInfo) =>
    delaySpec(o).store === 'sendspin' ? (delayMap[o.node_name] ?? 0) : (o.latency_ms ?? o.latency_effective_ms ?? 0);

  /** Where this output's current value came from, for the note's parenthetical. */
  const originOf = (o: OutputInfo) =>
    delaySpec(o).store === 'sendspin'
      ? appliedMs(o) === 0
        ? 'no trim'
        : 'your trim'
      : o.latency_ms == null
        ? `default ${o.latency_effective_ms ?? 0} ms`
        : 'your override';

  // The drag machinery (ownership, throttle, popover) lives in DelaySlider — this
  // page only says what each kind's knob *means* and where its value is stored.
  //
  // `applyOutputs` no longer re-seeds a slider from the pushed listing at all: the
  // component holds its own position while the thumb is the user's, so the listing is
  // free to arrive whenever it likes.
  async function commitDelay(o: OutputInfo, ms: number) {
    const spec = delaySpec(o);
    await run(
      () => (spec.store === 'sendspin' ? api.setSendspinDelay(o.node_name, ms) : api.setOutputLatency(o.node_name, ms)),
      `Set '${o.name}' ${spec.label.toLowerCase()} to ${ms} ms`,
    );
    // Adopt whatever the daemon actually stored — it clamps, and sendspin delays are
    // fetched rather than pushed, so without this the row could keep showing a value
    // no device is running.
    await refresh();
  }

  // Silent on purpose: this fires several times a second while dragging, and `run()`
  // would raise a toast for each. The commit above reports, and a failure there says
  // the same thing about the same endpoint.
  const liveDelay = (o: OutputInfo, ms: number) =>
    void (delaySpec(o).store === 'sendspin' ? api.setSendspinDelay(o.node_name, ms) : api.setOutputLatency(o.node_name, ms)).catch(
      () => {},
    );

  // Drop the override so the output follows the daemon default again. The slider
  // has no "empty" position, so clearing needs its own control.
  async function resetDelay(o: OutputInfo) {
    if (await run(() => api.setOutputLatency(o.node_name, null), `Reset '${o.name}' ${delaySpec(o).label.toLowerCase()} to default`)) {
      await refresh(); // the slider adopts the restored default from the listing
    }
  }

  // True only until the first listing arrives. A re-read must not put the page
  // back into its loading card: with the fingerprint-driven refreshes below, that
  // would blank the whole list every time a device appeared or a level changed.
  let everLoaded = $state(false);

  // Sendspin static delays are the one piece not pushed over the socket, so the last
  // fetched map is kept — to re-seed the editable fields when a listing arrives, and
  // as the "applied" readout while that slider is being dragged. `$state` because the
  // template reads it now: a plain variable would leave that readout stale until
  // something unrelated happened to re-render the row.
  let delayMap = $state<Record<string, number>>({});

  /** Adopted list → local state. */
  function applyOutputs(outs: OutputInfo[]) {
    outputs = outs;
    // Nothing to seed: each slider holds its own position (DelaySlider) and reads the
    // stored value straight off this listing, so a push arriving mid-drag is harmless.
  }

  /** Offered list → local state, present devices first: a long discovered list on
   * a busy network is mostly things reachable *now*, and those are the ones you
   * can identify with a test tone. */
  function applyOffered(disc: OutputInfo[]) {
    offered = [...disc].sort((a, b) => Number(b.present) - Number(a.present) || a.name.localeCompare(b.name));
  }

  async function refresh() {
    loading = !everLoaded;
    try {
      const [outs, disc, delays] = await Promise.all([
        api.outputs(),
        api.discoveredOutputs().catch(() => [] as OutputInfo[]),
        api.sendspinDelays().catch(() => ({}) as Record<string, number>),
      ]);
      delayMap = delays;
      applyOutputs(outs);
      applyOffered(disc);
    } catch {
      outputs = [];
      offered = [];
    }
    everLoaded = true;
    loading = false;
  }
  onMount(refresh);

  // Live listings with no polling and no second round-trip: the daemon pushes each
  // listing on the routing socket when it changes (bridge-daemon/src/routing.rs),
  // so this adopts the payload rather than re-fetching. The fetch above stays as
  // the first paint; a daemon too old to push simply never lands here.
  $effect(() => {
    const pushed = $routing.outputs;
    untrack(() => {
      if (pushed) applyOutputs(pushed);
    });
  });
  $effect(() => {
    const pushed = $routing.discovered;
    untrack(() => {
      if (pushed) applyOffered(pushed);
    });
  });

  // Outputs whose music is ducked right now → the gain in force on them
  // (`GET /api/duck`). Voice ducking is driven by the Home Assistant integration
  // (it resolves which room a satellite is in); this tab only reflects it, so a
  // speaker that sounds quiet is explainable without reading the daemon log.
  // On its own short interval, because a hold lasts one voice turn — a few
  // seconds — and would otherwise almost never be on screen.
  let ducks = $state<Record<string, number>>({});
  async function refreshDucks() {
    try {
      const holds = await api.duckHolds();
      const next: Record<string, number> = {};
      for (const h of holds) {
        // Several holds can overlap one output (two satellites, or a voice turn
        // plus an announcement); the daemon mixes at the strongest, so show that.
        const seen = next[h.output];
        next[h.output] = seen == null ? h.level : Math.min(seen, h.level);
      }
      ducks = next;
    } catch {
      ducks = {};
    }
  }
  onMount(refreshDucks);
  const duckPoll = setInterval(refreshDucks, 2000);
  onDestroy(() => clearInterval(duckPoll));

  // "Ducked" badge: this output's music is attenuated right now with no clip of
  // its own — a voice assistant in its room is talking (the integration's
  // voice_duck.py). Informational, not a warning: it is the feature working.
  function duckBadge(o: OutputInfo): { text: string; title: string } | null {
    const level = ducks[o.node_name];
    if (level == null) return null;
    const pct = Math.round(level * 100);
    return {
      text: `ducked ${pct}%`,
      title:
        `Music on this output is playing at ${pct}% while a voice assistant in its room is talking. ` +
        'Held by the Home Assistant integration and released when the turn ends; if that holder goes ' +
        'away, the add-on un-ducks by itself when the lease expires.',
    };
  }

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

  // A receiver host is added by *pairing* it, so its button says so and its card
  // shows the code to check first. Same call either way — the daemon mints the
  // token as part of adopting (plan §8), which is why there is no separate
  // pairing step to click.
  const isUnpaired = (o: OutputInfo) => o.kind === 'pwsink' && o.pwsink_paired === false;
  const addLabel = (o: OutputInfo) => (isUnpaired(o) ? 'Pair' : 'Add');
  // Pairing hands the token to a live agent, so there has to be one. This is the
  // ignored-then-stopped case: the row is remembered, the machine is not there.
  const cannotPairYet = (o: OutputInfo) => isUnpaired(o) && !o.present;

  // Removing is destructive of configuration (routing links, group membership,
  // the HA media_player), and the daemon can't undo it, so ask first. A device
  // that's still on the network comes back as a discovered offer.
  //
  // For a receiver host this is **Unpair**: it also revokes the token, because
  // "take this out of my outputs" and "stop trusting that machine" are not two
  // things a user wants separately. Its agent keeps dialling in, so it returns
  // below as pairable — ignore it there to put it away for good.
  async function remove(o: OutputInfo) {
    if (o.kind === 'pwsink') {
      const ok = await askConfirm({
        title: `Unpair '${o.name}'?`,
        body: [
          'Its pairing is revoked, and its routing, group membership and Home Assistant media_player are removed.',
          "The agent on that machine keeps asking to pair, so it comes back below as a discovered device — ignore it there if you don't want it back.",
        ],
        confirmLabel: 'Unpair',
        danger: true,
      });
      if (!ok) return;
      return decide(o, () => api.unpairOutput(o.node_name));
    }
    if (!(await askConfirm(removeOutputConfirm(o.name, o.present)))) return;
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
  // offering the control otherwise.
  //
  // Not asked about, because it is exactly undoable: the name we're dropping is
  // the one on screen, so putting it back is the same call with it. The old
  // *reason* for asking — that the device's announced name is shown nowhere, so
  // you couldn't type your way back — is what the Undo answers.
  async function resetName(o: OutputInfo) {
    const previous = o.name;
    const restore = async () => {
      const res = await api.renameOutput(o.node_name, previous);
      if (res.ok === false) throw new Error(res.message ?? `Could not restore '${previous}'`);
      await refresh();
    };
    await runUndoable(
      () => api.renameOutput(o.node_name, null),
      `Dropped '${previous}' — using the name this device announces`,
      restore,
      `Named '${previous}' again`,
    );
    await refresh();
  }

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

  // ---- Microphone-assisted alignment (plan §12.1) --------------------------------
  //
  // **This is the wizard's home**, and the reason it is here rather than on a source
  // card: a run does not align "the speakers playing this source", it aligns *a set of
  // speakers the user picks*, and the daemon forms a temporary group around exactly that
  // set. The choice being made is a choice about speakers, so it belongs on the page that
  // lists them. Moving it here also removes the second entry point — there is one
  // alignment session process-wide, and the source card no longer offers to start one
  // (see the comment at the top of AlignPanel.svelte).
  //
  // Mounted as one tag because the wizard is self-contained: `group` is only a seed for
  // the selection and there is nothing to seed it with here — the user picks the scope on
  // the wizard's own Speakers page, which is the model §12.1 asked for.
  let wizardOpen = $state(false);

  // A measured run stays *revertable* after the wizard is closed (plan §9.4), and this is
  // now the page it was started from — so the undo has to be reachable here. One status
  // read on mount rather than `measure.attach()`: attaching would open the push socket and
  // poll for as long as this page is open, on a page most visits never align from.
  onMount(() => void measure.refreshOnce());
  const revertScope = $derived(measure.canRevert ? measure.revertScope : []);

  /** Friendly name for a node name, the same resolution the routing graph uses (the
   *  rename store first). The wizard needs one for every speaker it names, including
   *  sources it never lists. */
  function alignLabel(nodeName: string): string {
    const matrix = [...$routing.matrix.outputs, ...$routing.matrix.sources];
    return (
      matrix.find((n) => n.node_name === nodeName)?.display_name ??
      outputs.find((o) => o.node_name === nodeName)?.name ??
      nodeName
    );
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

<!-- The per-output delay control: AirPlay 2's render delay, a PipeWire host's
     playout delay, a sendspin speaker's static trim. One slider for all three
     (`delaySpec` carries what differs) rather than a number box: this is a knob you
     hunt for the edge of by ear, and the coloured track shows which way is risky
     before you drag there. Self-guarding like `codecField`, and a snippet because
     `{@const}` needs a block to live in. -->
{#snippet delayField(o: OutputInfo)}
  {#if hasDelayKnob(o)}
    {@const spec = delaySpec(o)}
    <DelaySlider
      id="sync-{o.node_name}"
      label={spec.label}
      applied={appliedMs(o)}
      min={spec.min}
      max={spec.max}
      step={spec.step}
      riskyBelow={spec.riskyBelow}
      highAbove={spec.highAbove}
      risk={spec.risk}
      good={spec.good}
      origin={originOf(o)}
      live={spec.liveDuringDrag}
      deferredHint={spec.liveDuringDrag ? '' : ' — on release'}
      onLive={(ms) => liveDelay(o, ms)}
      oncommit={(ms) => commitDelay(o, ms)}
      onreset={spec.clearable ? () => resetDelay(o) : undefined}
      resetDisabled={o.latency_ms == null}
      resetTitle="Drop the override and follow the add-on default ({o.latency_effective_ms ?? 0} ms)"
    />
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

<!-- Speaker alignment. Its own card between the page's explanation and the output list,
     because it is about the *set* rather than about any one output — and because the set
     is picked inside the wizard (plan §12.1). Kept out of the loading branch so a run in
     progress, or an outstanding revert, is still reachable while the listings refresh. -->
{#if outputs.length >= 2 || wizardOpen || revertScope.length}
  <div class="card align-card">
    <div class="info-head">
      <h2>Timing between speakers</h2>
      {#if !wizardOpen}
        <div class="info-actions">
          <button
            class="ghost"
            type="button"
            title="Hold a phone where you listen and let the add-on measure each speaker's delay instead of judging it by ear"
            onclick={() => (wizardOpen = true)}
          >
            Measure with a microphone
          </button>
        </div>
      {/if}
    </div>
    <p class="card-sub" style="margin-bottom:0">
      Speakers playing one stream should land together, but each adds its own delay on the way to the cone. The wizard
      plays a click on the speakers you choose, measures when each one arrives at your phone, and proposes a setting per
      speaker — <strong>nothing is written until you approve it</strong>. The speakers you pick are taken over for the
      run, so whatever they are playing stops and comes back afterwards. Tuning one speaker <em>by ear</em> instead lives
      on the <strong>Sources</strong> page, on the card of the source it is playing.
    </p>

    {#if wizardOpen}
      <!-- One tag, no `group`: there is no source here to seed a selection from, and the
           scope is the user's choice on the wizard's own Speakers page. -->
      <AlignWizard label={alignLabel} onClose={() => (wizardOpen = false)} />
    {/if}

    <!-- Plan §9.4: the write is destructive to a previously-tuned setup, and the daemon
         keeps it revertable after the run is abandoned and the wizard closed — which is
         exactly when someone decides they preferred it before. -->
    {#if revertScope.length}
      <div class="revert">
        <span>A measurement wrote timing settings to {revertScope.map(alignLabel).join(', ')}.</span>
        <button class="ghost" disabled={measure.busy} onclick={() => void measure.revert()}>
          Revert to the settings from before
        </button>
        <span class="hint">
          Every one of them goes back to what it had before that run, and each speaker whose setting changes reconnects —
          so expect another quiet gap.
        </span>
      </div>
    {/if}
  </div>
{/if}

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
                 only once a name of your own is stored, and offers an Undo rather
                 than asking — see `resetName`. -->
            <h3>
              <GroupTitle
                name={o.name}
                minLength={MIN_OUTPUT_NAME_CHARS}
                title="Rename this output"
                onRename={(name) => rename(o, name)}
                onReset={o.renamed ? () => resetName(o) : undefined}
                resetTitle="Use the name this device announces"
              />
            </h3>
          </div>
          <!-- Badges sit at the right, in the order that keeps their columns
               aligned down the list: the two every output has (type, then
               status) are anchored against the volume control, and the optional
               ones hang off to their left rather than displacing them. "Ducked"
               is leftmost because it is the most transient — it comes and goes
               with a single voice turn, and should not shift the PTP badge. -->
          <div class="out-badges">
            {#if duckBadge(o)}
              {@const duck = duckBadge(o)!}
              <span class="badge duck" title={duck.title}>{duck.text}</span>
            {/if}
            <!-- A fault the daemon has diagnosed (see `last_error`). The badge is
                 only the marker — the sentence itself is printed below the header,
                 because a collapsed card would otherwise hide the one thing the
                 user needs to read. -->
            {#if o.last_error}
              <span class="badge warn" title={o.last_error}>fault</span>
            {/if}
            {#if ptpBadge(o)}
              {@const ptp = ptpBadge(o)!}
              <span class={ptp.cls} title={ptp.title}>{ptp.text}</span>
            {/if}
            <span class={st.cls} title={st.title}>{st.text}</span>
            <span class="badge">{kindLabel(o)}</span>
          </div>
          <!-- The volume column is kept even for an output that has no volume to
               control (an agent-less pw-sink host, or anything offline), so the badges
               beside it line up with every other row's instead of sliding right.
               Gated on *capability*, not on kind: the daemon reports `volume`/`muted`
               exactly when it can drive that output (sendspin and AP2 in-band, pw-sink
               through the receiver agent), and null when it genuinely cannot. Listing
               kinds here is what previously hid the control from every pw-sink host the
               daemon could already drive. -->
          <div class="out-vol">
            {#if o.present && hasLevelControl(o.node_name)}
              <VolumeControl
                percent={liveVol.get(o.node_name) ?? null}
                muted={muted[o.node_name] ?? false}
                onVolume={(pct) => onVolume(o, pct)}
                onMute={() => onMute(o)}
              />
            {/if}
          </div>
        </header>

        <!-- Outside the collapse guard on purpose: "this output cannot play, and
             here is why" is the reason you came to this page, and an offline card
             is exactly the one you find collapsed. -->
        {#if o.last_error}
          <p class="out-fault" role="status">{o.last_error}</p>
        {/if}

        {#if !isCollapsed(o)}
          {@render connMeta(o)}

          <!-- Guarded: an output kind with no tuning knobs at all would render an
               empty bordered row, which reads as broken. Every kind has a delay
               knob today, so this only guards against the next one that doesn't. -->
          {#if hasDelayKnob(o)}
          <div class="out-controls">
            {@render delayField(o)}

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
              <button
                class="danger"
                disabled={deciding[o.node_name]}
                title={o.kind === 'pwsink'
                  ? 'Revoke this host’s pairing and remove it from your outputs'
                  : 'Remove this output, its routing and its Home Assistant media_player'}
                onclick={() => remove(o)}
              >
                {#if deciding[o.node_name]}
                  {o.kind === 'pwsink' ? 'Unpairing…' : 'Removing…'}
                {:else}
                  {o.kind === 'pwsink' ? 'Unpair' : 'Remove'}
                {/if}
              </button>
            </div>
          </div>
        {/if}
      </article>
    {/each}
  {/if}

  <!-- One section for everything on offer, receiver hosts included: a host whose
       agent has dialled in but isn't paired yet *is* a discovered device, and
       pairing it is the Add. It used to have a section of its own above this one,
       which meant two decisions (pair, then add) for one intention. -->
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
          Nothing new. Compatible AirPlay 2 / Sendspin devices appear here automatically when they're on
          the network, and a Linux machine appears once its receiver agent is running.
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
        <!-- The pairing code, next to the button that uses it: this is the check
             that the host asking is the one you think it is, and it is worthless
             if you have to go looking for it. Same code that machine's own agent
             logged at startup, and it doesn't change while it reconnects. -->
        {#if o.pwsink_pair_code}
          <div class="pair-code" title="Pairing code — compare with the one this machine's agent logged">
            {o.pwsink_pair_code}
          </div>
        {/if}
        <!-- The decision itself is one click from the collapsed row: you
             shouldn't have to expand a card to dismiss a neighbour's speaker. -->
        <div class="out-decide">
          <!-- Pairing needs the host's agent on the socket to hand the token to, so
               an offline one (an ignored host whose agent has since stopped, say)
               says why instead of offering a button that can only fail. -->
          <button
            class="primary"
            disabled={deciding[o.node_name] || cannotPairYet(o)}
            title={cannotPairYet(o)
              ? 'Its agent is not connected — start pwrouter-agent on that machine first'
              : isUnpaired(o)
                ? 'Pair this machine and add it as an output — check the code first'
                : 'Add this device as an output'}
            onclick={() => add(o)}
          >
            {deciding[o.node_name] ? 'Working…' : addLabel(o)}
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
          {#if isUnpaired(o)}
            This machine's agent is asking to pair. Check the code above against the one it logged
            (<code>journalctl --user -u pwrouter-agent</code> on that machine) — approving a request you can't
            identify hands your audio to whoever else is on the network. Pairing it adds it as an output.
          {:else}
            Not routable and not in Home Assistant until you add it. Delay tuning and volume become available then.
          {/if}
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

  /* The alignment card. The wizard brings its own frame, so this only has to hold the
     header, the sentence and the revert offer. */
  .align-card .revert {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 10%, transparent);
    font-size: 0.82rem;
  }
  .align-card .revert button {
    padding: 4px 10px;
    font-size: 0.8rem;
  }
  .align-card .revert .hint {
    font-size: 0.78rem;
    color: var(--secondary-text-color);
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

  /* The pairing code, monospaced and spaced out: it exists to be read off one
     screen and compared with another, so legibility beats compactness. */
  .pair-code {
    flex: 0 0 auto;
    font-family: var(--code-font-family, monospace);
    font-size: 1.05rem;
    letter-spacing: 0.18em;
    padding: 4px 10px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--primary-color) 12%, transparent);
    color: var(--primary-color);
    white-space: nowrap;
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
  /* Ducking is an expected, transient state — informational, never an alarm. */
  .badge.duck {
    color: var(--info-color, #4285f4);
    border-color: currentColor;
  }
  /* The diagnosed reason an output can't play. Tinted rather than boxed in alarm
     colours: it is an explanation to read, not an alert to dismiss, and it sits
     under the header of a card that is already marked offline/fault. */
  .out-fault {
    margin: 0 0 8px;
    padding: 8px 10px;
    border-left: 3px solid var(--error-color, #db4437);
    border-radius: 4px;
    background: color-mix(in srgb, var(--error-color, #db4437) 8%, transparent);
    color: var(--primary-text-color);
    font-size: 0.85rem;
    line-height: 1.4;
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
  .delay-slider::-moz-range-thumb {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    border: 2px solid var(--card-background-color, #fff);
    background: var(--primary-text-color);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.35);
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
</style>
