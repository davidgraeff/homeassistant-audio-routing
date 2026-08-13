<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';
  import { api, MIN_OUTPUT_NAME_CHARS } from '../../lib/api';
  import { routing } from '../../lib/routing';
  import { run, runUndoable, toast } from '../../lib/toast';
  import { askConfirm, removeOutputConfirm } from '../../lib/confirm.svelte';
  import type { OpResponse, OutputInfo, SendspinCodec } from '../../lib/types';
  import { delaySpec, hasDelayKnob } from '../../lib/outputs/delay';
  import { levelCaps } from '../../lib/outputs/level';
  import { canTest, kindLabel, ptpBadge, statusBadge, syncBadge, testHint } from '../../lib/outputs/labels';
  import GroupTitle from '../groups/GroupTitle.svelte';
  import OutputsDocs from './OutputsDocs.svelte';
  import ReceiverAgentDocs from './ReceiverAgentDocs.svelte';
  import DelaySlider from '../ui/DelaySlider.svelte';
  import VolumeControl from '../ui/VolumeControl.svelte';

  // No props: this page owns its own listings, and alignment lives on its own page.

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

  /** What the daemon says it can drive per output — its own answer (`level_caps`),
   *  not this page's guess.
   *
   *  It used to be inferred from `volume`/`muted` being non-null, which is the question
   *  "has a level arrived?" — a near-miss that held only because sendspin and AirPlay 2
   *  always report some mute. Kind-agnostic either way, and now also independent of
   *  whether a level has ever been read: a receiver that has never told us its volume
   *  still has a knob. See `lib/outputs/level.ts`. */
  const caps = $derived(new Map($routing.matrix.outputs.map((o) => [o.node_name, levelCaps(o)])));
  const hasLevelControl = (nodeName: string) => {
    const c = caps.get(nodeName);
    return !!c && (c.volume || c.mute);
  };

  // Outputs a speaker-timing measurement holds right now (`RoutingNode.held`), from
  // the same live matrix as the levels above. That hold is *exclusive*: while it is up
  // those speakers play nothing, and this page is where someone stands when they
  // wonder why a room is quiet. Read off the pushed matrix rather than from the
  // alignment API, so it appears — and vanishes — with the hold itself; the notice
  // this replaced polled a session and could go on claiming a hold that had already
  // been released.
  const heldNow = $derived(new Set($routing.matrix.outputs.filter((o) => o.held).map((o) => o.node_name)));

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


  // The delay-knob specs live in lib/outputs/delay.ts; the display helpers in
  // lib/outputs/labels.ts. Both are pure — no component state.

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
      () => api.setOutputDelay(o.node_name, ms),
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
  const liveDelay = (o: OutputInfo, ms: number) => void api.setOutputDelay(o.node_name, ms).catch(() => {});

  // Drop the override so the output follows the daemon default again. The slider
  // has no "empty" position, so clearing needs its own control.
  async function resetDelay(o: OutputInfo) {
    if (await run(() => api.setOutputDelay(o.node_name, null), `Reset '${o.name}' ${delaySpec(o).label.toLowerCase()} to default`)) {
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

  // "Held" badge: an alignment run has this output for the duration, and the hold is
  // exclusive — so anything routed here is silent until it ends. The sentence names the
  // cause and stops there: no link, no invitation, nothing about where the feature
  // lives. Same register as "fault" and "not connected" — this row is reporting a
  // state, not advertising a page.
  function heldBadge(o: OutputInfo): { text: string; title: string } | null {
    if (!heldNow.has(o.node_name)) return null;
    return {
      text: 'held',
      title:
        'A speaker-timing measurement has taken this output over. Nothing else plays on it until that finishes, ' +
        'and whatever was routed here comes back afterwards.',
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

  // Resync: the same button for the same symptom on two transports — an output that is
  // demonstrably being *sent* audio and plays none. What it costs differs, so what it
  // sends does too:
  //
  // * **sendspin** — `stream/clear`: drop the buffered audio and re-anchor. One frame,
  //   no reconnect. Measured on 2026-08-03, when three of four devices went silent while
  //   the daemon, the graph and the clock sync were all healthy.
  // * **AirPlay 2** — release the RTSP session and build a fresh one, re-arming the
  //   receiver's PTP peer on the way. Heavier (a few seconds of silence on that
  //   receiver), and the only thing that recovers a receiver which has lost its clock
  //   lock — the Pioneer's failure mode, where the alternatives were restarting the
  //   add-on or power-cycling the AVR. The daemon also does this by itself when it can
  //   see the lock go (its watchdog); the button is for when it cannot, and for not
  //   waiting.
  //
  // Both are offered only for a present device: with nothing connected there is nothing
  // to resync, and the daemon says so rather than pretending.
  let clearing = $state<Record<string, boolean>>({});
  const canClear = (o: OutputInfo) => (o.kind === 'sendspin' || o.kind === 'airplay2') && o.present;
  const resyncHint = (o: OutputInfo) =>
    o.kind === 'airplay2'
      ? 'Release this receiver’s AirPlay session and build a fresh one, re-arming its PTP clock. Try this if it is connected but silent — its groupmates keep playing.'
      : 'Discard this speaker’s buffered audio and re-anchor it (stream/clear). Try this if it is connected but silent — it does not disturb the other speakers in its group.';
  async function resync(o: OutputInfo) {
    clearing = { ...clearing, [o.node_name]: true };
    try {
      // One endpoint, one intent: the daemon picks the mechanism its kind has.
      const res = await api.resyncOutput(o.node_name);
      toast(res.ok ? 'success' : 'error', res.message);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
    clearing = { ...clearing, [o.node_name]: false };
  }

  // Per-device volume / mute for every output kind that reports a level (sendspin,
  // AirPlay 2, and a pw-sink host through its agent), from the always-visible card
  // header. No local echo of the new level: the daemon pushes a matrix frame back, and
  // <VolumeControl>'s drag guard holds the thumb until it lands — so the value on
  // screen is always one the daemon confirmed.
  //
  // One endpoint for every kind, on one scale (0.0–1.0): this page used to pick per kind
  // through lib/outputs/level.ts, and picking wrongly is what sent pw-sink levels to the
  // sendspin endpoint, where they were stored for a device that does not exist and then
  // overwritten by the next frame.
  async function onVolume(o: OutputInfo, pct: number) {
    try {
      await api.setOutputVolume(o.node_name, pct / 100);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
  async function onMute(o: OutputInfo) {
    const next = !muted[o.node_name];
    muted = { ...muted, [o.node_name]: next }; // optimistic; the matrix confirms
    try {
      await api.setOutputMute(o.node_name, next);
    } catch (e) {
      // The optimistic flip has to go back: a host with no live agent answers 503,
      // and leaving the button "muted" would claim something that never happened.
      muted = { ...muted, [o.node_name]: !next };
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

  // ---- Speaker alignment: the way in, and the state (plan §12.1) ------------------
  //
  // **The wizard itself lives on the Alignment page**, not here. What is here is what a
  // user needs while looking at their speakers, and nothing more:
  //
  //   * the way in — aligning is a choice about *a set of speakers the user picks* (never
  //     "the speakers playing this source"), so the page that lists speakers is where the
  //     offer belongs;
  //   * whether an alignment is **holding** speakers right now. That hold is exclusive, so
  //     without this the user would be looking at speakers that are silent for a reason
  //     nothing on the page mentions;
  //   * the offer to **revert** a measurement that was written (§9.4), which outlives the
  //     run that wrote it and is exactly what someone wants when they decide they preferred
  //     the old timing.
  //
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
      title={resyncHint(o)}
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
  <div class="card-head">
    <h2>Supported outputs</h2>
    <div class="actions">
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
        Setup Linux/PipeWire host
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
            class="icon-toggle"
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
            <!-- Taken over by an alignment run: it sits between "ducked" and "fault"
                 because it lasts a whole run rather than a voice turn, and amber
                 (`caution`) because the output is not faulty — it just isn't carrying
                 what you routed to it. -->
            {#if heldBadge(o)}
              {@const held = heldBadge(o)!}
              <span class="badge caution" title={held.title}>{held.text}</span>
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
            <!-- The speaker's own verdict on whether it is rendering in step (sendspin
                 only, and only when it has something to report). The counterpart of the
                 PTP badge for the other transport. -->
            {#if syncBadge(o)}
              {@const sync = syncBadge(o)!}
              <span class={sync.cls} title={sync.title}>{sync.text}</span>
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
                canVolume={caps.get(o.node_name)?.volume ?? true}
                canMute={caps.get(o.node_name)?.mute ?? true}
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
          class="icon-toggle"
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

  /* The alignment card — the way in and the two notices. The wizard is a page of its own
     now, so this only has to hold the header, the sentence, the live-hold line and the
     revert offer. */
  /* A live hold, seen from the page that lists the speakers. Primary-coloured rather than
     amber: nothing is wrong, something is *running* — and it is the colour the wizard uses,
     so "Show it" leads somewhere that looks related. */

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
