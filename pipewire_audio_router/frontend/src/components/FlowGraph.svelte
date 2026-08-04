<script lang="ts">
  import { untrack, onDestroy } from 'svelte';
  import { routing } from '../lib/routing';
  import { api } from '../lib/api';
  import { run, toast } from '../lib/toast';
  import type { MusicGroup, RoutingNode } from '../lib/types';
  import VolumeControl from './VolumeControl.svelte';
  import RoutingHelp from './RoutingHelp.svelte';

  // Interactive bipartite routing graph: sources on the left, what they play on
  // on the right, links drawn as SVG curves between them. Drag from a handle to
  // one on the opposite side to route; click a curve to remove that route.
  //
  // The right column is **music groups** (each showing its speakers), plus one
  // node per ungrouped speaker. That's deliberate: a wire onto a group is the
  // same operation as that group's `Source` dropdown (`routeMusicGroup`), so the
  // two surfaces can't disagree, and the UI can't create the "⚠ Mixed" state
  // where a group's members sit on different sources. Mixed state that already
  // exists (from the API, or from the expert view below) is still *shown* — as a
  // dashed partial wire per source, which is exactly what it is.
  //
  // "Show individual speakers" drops back to the raw per-output matrix: every
  // speaker its own node, wired one at a time. That's the diagnosis/escape-hatch
  // view — the only way to route a member of a group by itself.

  interface Props {
    /** Music groups (from the caller, which already owns their CRUD). Empty ⇒
     *  the graph degenerates to the per-speaker view. */
    groups?: MusicGroup[];
  }
  let { groups = [] }: Props = $props();

  const SENDSPIN_DEV_PREFIX = 'sendspin-dev-';
  const AP2_DEV_PREFIX = 'ap2-dev-';
  // Virtual outputs (sendspin + AirPlay-2) carry volume/mute in-band; both are
  // driven entirely by the routing matrix over the WebSocket (no polling). RAOP
  // (AirPlay 1) is being retired and no longer exposes volume in this UI.
  const isVirtual = (name: string) => name.startsWith(SENDSPIN_DEV_PREFIX) || name.startsWith(AP2_DEV_PREFIX);

  /** Expert view: bypass groups and wire individual speakers. */
  let showSpeakers = $state(false);

  // The graph is the lower-altitude view of routing the group cards above already
  // express, so it starts folded away and the choice is remembered — a returning
  // user gets the page they left, not the page we guessed.
  const OPEN_KEY = 'par-graph-open';
  let open = $state(localStorage.getItem(OPEN_KEY) === '1');
  $effect(() => {
    localStorage.setItem(OPEN_KEY, open ? '1' : '0');
  });

  // The explanation, in a dialog behind the Explain button.
  let helpOpen = $state(false);
  let helpEl = $state<HTMLDivElement>();
  $effect(() => {
    if (helpOpen) helpEl?.focus();
  });

  // Fixed geometry — every row is an exact pixel height, so handle positions are
  // pure arithmetic (no per-frame DOM measurement); only the canvas width is
  // measured (it drives where the right column sits).
  const COL_W = 240; // node-card width
  const ROW_SRC = 52; // source card height
  const GAP = 16; // vertical gap between cards
  const TOP = 8; // top padding
  const HIT = 80; // drop hit-test radius (px) around a target handle
  const PAD = 8; // vertical padding inside a target card
  const HEAD_H = 22; // a group's title row
  const MEM_VOL_H = 42; // speaker row carrying a volume slider
  const MEM_H = 22; // speaker row without one
  const MEM_GAP = 6; // gap between speaker rows

  let canvasEl: HTMLDivElement | undefined = $state();
  let Wc = $state(0); // measured canvas width
  let dragging = $state<{ kind: 'source' | 'target'; name: string; x: number; y: number } | null>(null);

  const S = $derived($routing.matrix.sources);
  const O = $derived($routing.matrix.outputs);
  const links = $derived($routing.matrix.links);

  const srcInfo = $derived(new Map(S.map((n) => [n.node_name, n])));
  const outInfo = $derived(new Map(O.map((n) => [n.node_name, n])));

  // ---- right column: groups + loose speakers (or raw speakers) -------------
  // A target is one node in the right column and one routing endpoint: either a
  // whole music group (routed as a unit) or a single speaker.
  type Target =
    | { kind: 'group'; key: string; id: string; name: string; members: RoutingNode[] }
    | { kind: 'solo'; key: string; name: string; members: RoutingNode[] };

  /** A group member the matrix doesn't know (never routed, never seen). */
  function placeholder(nodeName: string): RoutingNode {
    return { node_name: nodeName, display_name: nodeName, present: false, configured: true, node_id: null, peak: 0 };
  }
  const outNode = (name: string) => outInfo.get(name) ?? placeholder(name);
  const solo = (n: RoutingNode): Target => ({ kind: 'solo', key: n.node_name, name: n.display_name, members: [n] });

  const targets = $derived.by<Target[]>(() => {
    if (showSpeakers) return O.map(solo);
    const grouped = new Set(groups.flatMap((g) => g.members));
    return [
      ...groups.map((g) => ({
        kind: 'group' as const,
        key: `g:${g.id}`,
        id: g.id,
        name: g.name,
        members: g.members.map(outNode),
      })),
      ...O.filter((o) => !grouped.has(o.node_name)).map(solo),
    ];
  });

  const memberH = (n: RoutingNode) => (n.present && isVirtual(n.node_name) ? MEM_VOL_H : MEM_H);
  function targetH(t: Target): number {
    const rows = t.members.length ? t.members.map(memberH) : [MEM_H];
    const body = rows.reduce((a, b) => a + b, 0) + (rows.length - 1) * MEM_GAP;
    return 2 * PAD + body + (t.kind === 'group' ? HEAD_H + MEM_GAP : 0);
  }

  // Handle centers, by node name / target key. Source handles sit on the right
  // edge of the left column; target handles on the left edge of the right one.
  const srcPos = $derived(
    S.map((n, i) => ({ name: n.node_name, x: COL_W, y: TOP + i * (ROW_SRC + GAP) + ROW_SRC / 2 })),
  );
  const outX = $derived(Math.max(COL_W, Wc - COL_W));
  // Stacked top-to-bottom, each node as tall as its content needs.
  const layout = $derived.by(() => {
    let y = TOP;
    return targets.map((t) => {
      const h = targetH(t);
      const box = { t, top: y, h, x: outX, y: y + h / 2, name: t.key };
      y += h + GAP;
      return box;
    });
  });
  const srcByName = $derived(new Map(srcPos.map((p) => [p.name, p])));
  const targetByKey = $derived(new Map(layout.map((b) => [b.t.key, b])));

  const srcColH = $derived(S.length * (ROW_SRC + GAP));
  const outColH = $derived(layout.reduce((a, b) => a + b.h + GAP, 0));
  const canvasH = $derived(Math.max(120, TOP * 2 + Math.max(srcColH, outColH) - GAP));

  function bezier(x1: number, y1: number, x2: number, y2: number): string {
    const dx = Math.max(40, Math.abs(x2 - x1) * 0.4) * Math.sign(x2 - x1 || 1);
    return `M${x1},${y1} C${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`;
  }

  /** Which of `t`'s members are linked to `source`. */
  const linkedMembers = (t: Target, source: string) =>
    t.members.filter((m) => links.some((l) => l.source === source && l.output === m.node_name));

  /** Is this output actually taking delivery — not merely reachable? `streaming`
   *  is absent for the backends where the question doesn't apply (a sendspin
   *  device always has a sender), so only an explicit `false` means "nothing is
   *  attached to it". See `RoutingNode.streaming`. */
  const delivering = (n: RoutingNode) => n.present && n.streaming !== false;

  // Drawable edges: one per (source, target) pair with at least one link. A
  // `partial` edge means only some of a group's members are on that source —
  // the honest rendering of a mixed group. `waiting` means the route is up and
  // the endpoints are reachable, but no member has a live session, so nothing is
  // being delivered — drawn solid rather than animated, since an animated wire to
  // an output that can't hear it is exactly the lie the announce arbiter catches.
  type Edge = { source: string; target: Target; partial: boolean; off: boolean; waiting: boolean; path: string };
  const edges = $derived.by<Edge[]>(() => {
    const out: Edge[] = [];
    for (const box of layout) {
      const sources = new Set(
        links.filter((l) => box.t.members.some((m) => m.node_name === l.output)).map((l) => l.source),
      );
      for (const source of sources) {
        const a = srcByName.get(source);
        if (!a) continue;
        const linked = linkedMembers(box.t, source);
        const off = !(srcInfo.get(source)?.present ?? false) || !linked.some((m) => m.present);
        out.push({
          source,
          target: box.t,
          partial: linked.length !== box.t.members.length,
          off,
          waiting: !off && !linked.some(delivering),
          path: bezier(a.x, a.y, box.x, box.y),
        });
      }
    }
    return out;
  });

  /** Whether a group's members disagree about what they're playing. */
  function isMixed(t: Target): boolean {
    if (t.kind !== 'group' || t.members.length === 0) return false;
    const sources = new Set(
      links.filter((l) => t.members.some((m) => m.node_name === l.output)).map((l) => l.source),
    );
    if (sources.size === 0) return false;
    return sources.size > 1 || linkedMembers(t, [...sources][0]).length !== t.members.length;
  }

  // Animate a routed wire while its source is carrying signal. `peak` arrives as
  // live WebSocket frames; we hold the "flowing" state for a short while past the
  // last above-threshold frame so brief quiet passages in music don't make the
  // animation stutter or drop out. Threshold matches when the meter-fill shows.
  const FLOW_THRESH = 0.02; // peak level (0–1) that counts as signal present
  const FLOW_HOLD = 1200; // ms to keep flowing after signal falls below threshold
  let flowing = $state<Record<string, boolean>>({});
  let flowTimers: Record<string, ReturnType<typeof setTimeout>> = {};

  $effect(() => {
    const srcs = $routing.matrix.sources;
    untrack(() => {
      for (const s of srcs) {
        if (!(s.present && (s.peak ?? 0) > FLOW_THRESH)) continue;
        if (!flowing[s.node_name]) flowing = { ...flowing, [s.node_name]: true };
        clearTimeout(flowTimers[s.node_name]); // extend the hold on each active frame
        flowTimers[s.node_name] = setTimeout(() => {
          flowing = { ...flowing, [s.node_name]: false };
        }, FLOW_HOLD);
      }
    });
  });
  onDestroy(() => {
    for (const t of Object.values(flowTimers)) clearTimeout(t);
    for (const t of Object.values(xrunTimers)) clearTimeout(t);
  });

  const ghost = $derived.by(() => {
    if (!dragging || Wc === 0) return null;
    const o = dragging.kind === 'source' ? srcByName.get(dragging.name) : targetByKey.get(dragging.name);
    return o ? bezier(o.x, o.y, dragging.x, dragging.y) : null;
  });

  function pointerXY(e: PointerEvent): { x: number; y: number } {
    const r = canvasEl?.getBoundingClientRect();
    return r ? { x: e.clientX - r.left, y: e.clientY - r.top } : { x: 0, y: 0 };
  }

  function startDrag(kind: 'source' | 'target', name: string, e: PointerEvent) {
    e.preventDefault();
    dragging = { kind, name, ...pointerXY(e) };
  }
  function onMove(e: PointerEvent) {
    if (dragging) dragging = { ...dragging, ...pointerXY(e) };
  }
  async function onUp(e: PointerEvent) {
    if (!dragging) return;
    const drag = dragging;
    dragging = null;
    const { x, y } = pointerXY(e);
    const candidates: { name: string; x: number; y: number }[] = drag.kind === 'source' ? layout : srcPos;
    let best: { name: string } | null = null;
    let bestD = HIT * HIT;
    for (const t of candidates) {
      const d = (t.x - x) ** 2 + (t.y - y) ** 2;
      if (d <= bestD) {
        bestD = d;
        best = t;
      }
    }
    if (!best) return;
    const source = drag.kind === 'source' ? drag.name : best.name;
    const targetKey = drag.kind === 'source' ? best.name : drag.name;
    const target = targetByKey.get(targetKey)?.t;
    if (!target) return;
    await route(source, target);
  }

  /** Route `source` to a whole target. Groups go through the group endpoint (the
   *  same reconciling call as their Source dropdown: every member on that one
   *  source, any other source removed); a lone speaker takes an extra link, so
   *  two sources can be mixed into one speaker deliberately. */
  async function route(source: string, target: Target) {
    const linked = linkedMembers(target, source);
    if (linked.length === target.members.length) return; // already routed
    if (target.kind === 'group') {
      await run(() => api.routeMusicGroup(target.id, source), `"${target.name}" now playing ${disp(srcInfo, source)}`);
    } else {
      await run(() => api.link(source, target.members[0].node_name));
    }
  }

  const disp = (map: Map<string, RoutingNode>, name: string) => map.get(name)?.display_name ?? name;

  // Estimated buffering (ms) a node adds — the configured jitter/playout buffer
  // the daemon reports (routing.rs `latency_ms`), not a measured figure.
  const fmtLat = (ms: number | null | undefined): string | null => (ms == null ? null : `~${ms} ms`);

  // Whether any node carries a latency estimate — gates the explanation caption.
  const anyLatency = $derived([...S, ...O].some((n) => n.latency_ms != null));

  // Per-node xrun (dropped-cycle) counts from the profiler — pw-top's ERR. The
  // count is cumulative, so a non-zero value alone means "has dropped at some
  // point"; what matters is whether it's climbing *now*. Hold a "hot" flag for a
  // short while after each increase so an actively-stuttering node lights up red
  // (mirrors the wire-flow hold above). `null` xruns = profiling off / virtual
  // output → no badge.
  const XRUN_HOT_HOLD = 2500; // ms to keep a node flagged after its count rises
  let xrunHot = $state<Record<string, boolean>>({});
  let prevXruns: Record<string, number> = {};
  let xrunTimers: Record<string, ReturnType<typeof setTimeout>> = {};
  const anyXruns = $derived([...S, ...O].some((n) => (n.xruns ?? 0) > 0));

  $effect(() => {
    const nodes = [...$routing.matrix.sources, ...$routing.matrix.outputs];
    untrack(() => {
      for (const n of nodes) {
        if (n.xruns == null) continue;
        const prev = prevXruns[n.node_name];
        if (prev != null && n.xruns > prev) {
          if (!xrunHot[n.node_name]) xrunHot = { ...xrunHot, [n.node_name]: true };
          clearTimeout(xrunTimers[n.node_name]);
          xrunTimers[n.node_name] = setTimeout(() => {
            xrunHot = { ...xrunHot, [n.node_name]: false };
          }, XRUN_HOT_HOLD);
        }
        prevXruns[n.node_name] = n.xruns;
      }
    });
  });

  /** Click a wire to remove that route — for a group, from every member on it. */
  async function removeEdge(e: Edge) {
    const what = e.target.kind === 'group' ? `group ${e.target.name}` : e.target.name;
    if (!confirm(`Remove route: ${disp(srcInfo, e.source)} → ${what}?`)) return;
    const members = linkedMembers(e.target, e.source);
    for (const m of members) {
      if (!(await run(() => api.unlink(e.source, m.node_name)))) return;
    }
  }

  async function forget(node: RoutingNode) {
    if (!confirm(`Remove '${node.display_name}'? It's offline; its saved routing will be forgotten (a real device reappears unrouted).`)) return;
    await run(() => api.forgetEntity(node.node_name), `Forgot '${node.display_name}'`);
  }

  // Same ✕, different meaning on an output: an output is in this matrix because
  // it was *added* on the Outputs page, so merely forgetting its routing would
  // leave the row sitting there and the ✕ would look broken. Removing un-adds it
  // — routing, group membership and Home Assistant entity go with it, and a
  // device that's still on the network reappears as a discovered offer.
  async function removeOutput(node: RoutingNode) {
    if (
      !confirm(
        `Remove '${node.display_name}' from your outputs? It's offline. Its saved routing, group membership and Home Assistant media_player are removed; if the device turns up again it appears on the Outputs page as a discovered device.`,
      )
    )
      return;
    await run(() => api.removeOutput(node.node_name), `Removed '${node.display_name}'`);
  }

  // Speakers sharing a source-set play as one synchronized group in the daemon —
  // which is what a music group's routing produces, so the badge only adds
  // information in the per-speaker view.
  const syncGroupOf = $derived.by(() => {
    const sourceKey = (outputName: string) =>
      links
        .filter((l) => l.output === outputName)
        .map((l) => l.source)
        .sort()
        .join('');
    const byKey = new Map<string, string[]>();
    for (const o of O) {
      if (!o.node_name.startsWith(SENDSPIN_DEV_PREFIX)) continue;
      const k = sourceKey(o.node_name);
      if (!k) continue;
      (byKey.get(k) ?? byKey.set(k, []).get(k)!).push(o.node_name);
    }
    const result = new Map<string, number>();
    let n = 0;
    for (const members of byKey.values()) {
      if (members.length < 2) continue;
      n += 1;
      for (const name of members) result.set(name, n);
    }
    return result;
  });

  // Per-device mute for virtual outputs (sendspin + AirPlay-2), keyed by node
  // name. Carried in the routing matrix (RoutingNode.muted) and pushed live over
  // the WebSocket — no polling. The volume value itself is passed straight to
  // <VolumeControl> from RoutingNode.volume (0–1, null = unknown → 0); that
  // component owns the drag guard so a stale live frame can't yank the thumb.
  let muted = $state<Record<string, boolean>>({});

  // Mirror mute state from the live matrix into the local map. `untrack`
  // reads/writes the map without making this effect depend on its own output.
  $effect(() => {
    const outs = $routing.matrix.outputs;
    untrack(() => {
      let mNext = muted;
      let mChanged = false;
      for (const o of outs) {
        if (!isVirtual(o.node_name)) continue;
        if (typeof o.muted === 'boolean' && mNext[o.node_name] !== o.muted) {
          if (!mChanged) mNext = { ...muted };
          mNext[o.node_name] = o.muted;
          mChanged = true;
        }
      }
      if (mChanged) muted = mNext;
    });
  });

  async function onVolume(nodeName: string, pct: number) {
    try {
      if (nodeName.startsWith(AP2_DEV_PREFIX)) await api.setAp2Volume(nodeName, pct / 100);
      else await api.setSendspinVolume(nodeName, pct);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
  async function onMute(nodeName: string) {
    const next = !muted[nodeName];
    muted = { ...muted, [nodeName]: next }; // optimistic; matrix confirms
    try {
      if (nodeName.startsWith(AP2_DEV_PREFIX)) await api.setAp2Mute(nodeName, next);
      else await api.setSendspinMute(nodeName, next);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
</script>

<svelte:window onpointermove={onMove} onpointerup={onUp} onkeydown={(e) => e.key === 'Escape' && (helpOpen = false)} />

<!-- Folded away by default: the title row is the whole card until you open it,
     and the explanation is a dialog behind the Explain button. -->
<div class="card graph-card">
  <div class="card-head">
    <h2>
      <button class="collapse-toggle" type="button" aria-expanded={open} aria-controls="routing-graph" title={open ? 'Hide the routing graph' : 'Show the routing graph'} onclick={() => (open = !open)}>
        <span class="chevron">▶</span>
        Routing graph
      </button>
    </h2>
    {#if open}
      <div class="actions">
        <label class="expert">
          <input type="checkbox" bind:checked={showSpeakers} />
          Show individual speakers
        </label>
        <button class="ghost help-btn" type="button" title="How to read and edit this graph" onclick={() => (helpOpen = true)}>
          Explain
        </button>
      </div>
    {/if}
  </div>

  {#if open}
    <div id="routing-graph">
      {#if O.length === 0}
        <p class="empty">No speakers available yet — add one under the Outputs tab.</p>
      {:else if S.length === 0}
        <p class="empty">No sources present right now (nothing is playing into the router).</p>
      {:else}
        <div class="flow">
          <div
            class="canvas"
            class:dragging={!!dragging}
            bind:this={canvasEl}
            bind:clientWidth={Wc}
            style="height:{canvasH}px"
          >
            <svg class="wires" width={Wc} height={canvasH}>
              {#each edges as e (e.source + ' ' + e.target.key)}
                <!-- svelte-ignore a11y_click_events_have_key_events -->
                <path class="hit" d={e.path} onclick={() => removeEdge(e)} role="button" tabindex="-1" aria-label="remove route"></path>
                <path
                  class="wire"
                  class:off={e.off}
                  class:partial={e.partial}
                  class:waiting={e.waiting}
                  class:active={flowing[e.source] && !e.off && !e.waiting}
                  d={e.path}
                ></path>
              {/each}
              {#if ghost}<path class="ghost" d={ghost}></path>{/if}
            </svg>

            {#each S as n, i (n.node_name)}
              <div class="node src" class:offline={!n.present} style="top:{TOP + i * (ROW_SRC + GAP)}px; height:{ROW_SRC}px; width:{COL_W}px">
                <div class="body">
                  <span class="nm" title={n.display_name}>{n.display_name}</span>
                  {#if !n.present}
                    <span class="tag off">offline</span>
                    <button class="x" title="Forget saved routing" onclick={() => forget(n)}>✕</button>
                  {:else}
                    <div class="meter" title="input level {Math.round(n.peak * 100)}%">
                      <div class="meter-fill" style="width:{Math.min(100, Math.round(n.peak * 100))}%"></div>
                    </div>
                    {#if fmtLat(n.latency_ms)}
                      <span class="lat" title="Estimated input jitter buffer this source adds">{fmtLat(n.latency_ms)}</span>
                    {/if}
                    {#if (n.xruns ?? 0) > 0}
                      <span class="xrun" class:hot={xrunHot[n.node_name]} title="Dropped audio cycles (PipeWire xruns) since this node started — pw-top's ERR. Red = climbing now, i.e. dropping out.">⚠ {n.xruns}</span>
                    {/if}
                  {/if}
                </div>
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <div
                  class="handle right"
                  role="button"
                  tabindex="-1"
                  aria-label="Drag to a group to route"
                  title="Drag to a group to route"
                  onpointerdown={(e) => startDrag('source', n.node_name, e)}
                ></div>
              </div>
            {/each}

            {#each layout as box (box.t.key)}
              {@const t = box.t}
              <div class="node out" class:group={t.kind === 'group'} style="top:{box.top}px; height:{box.h}px; width:{COL_W}px">
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <div
                  class="handle left"
                  role="button"
                  tabindex="-1"
                  aria-label="Drag to a source to route"
                  title="Drag to a source to route"
                  onpointerdown={(e) => startDrag('target', t.key, e)}
                ></div>
                <div class="tbody">
                  {#if t.kind === 'group'}
                    {@const mixed = isMixed(t)}
                    <div class="ghead" style="height:{HEAD_H}px">
                      <span class="nm" title={t.name}>{t.name}</span>
                      {#if mixed}
                        <span class="tag mix" title="Members are on different sources — pick this group's source again to reconcile">⚠ mixed</span>
                      {:else}
                        <!-- The speaker count is the low-value item: drop it rather
                             than squeeze the name when the mixed pill is present. -->
                        <span class="cnt">{t.members.length} {t.members.length === 1 ? 'speaker' : 'speakers'}</span>
                      {/if}
                    </div>
                  {/if}
                  {#if t.members.length === 0}
                    <div class="member empty-row" style="height:{MEM_H}px"><span class="cnt">no speakers yet</span></div>
                  {/if}
                  {#each t.members as m (m.node_name)}
                    <div class="member" class:offline={!m.present} style="height:{memberH(m)}px">
                      <span class="head-name">
                        <span class="nm" title={m.display_name}>{m.display_name}</span>
                        {#if showSpeakers && syncGroupOf.get(m.node_name)}
                          <span class="tag grp" title="Plays in sync with the other speakers on this source set">sync {syncGroupOf.get(m.node_name)}</span>
                        {/if}
                        {#if fmtLat(m.latency_ms)}
                          <span class="lat" title="Estimated playout buffer this speaker adds (group lead + any per-device delay)">{fmtLat(m.latency_ms)}</span>
                        {/if}
                        {#if (m.xruns ?? 0) > 0}
                          <span class="xrun" class:hot={xrunHot[m.node_name]} title="Dropped audio cycles (PipeWire xruns) since this node started — pw-top's ERR. Red = climbing now.">⚠ {m.xruns}</span>
                        {/if}
                        {#if !m.present}
                          <span class="tag off">offline</span>
                          <button class="x" title="Remove this output" onclick={() => removeOutput(m)}>✕</button>
                        {:else if m.streaming === false}
                          <!-- Reachable but nothing attached: distinct from offline,
                               and the reason an announcement here would be refused. -->
                          <span
                            class="tag wait"
                            title="On the network, but no session is up — nothing routed here is being played. A PipeWire target has to connect to us (its module-rtp-session initiates the handshake); an AirPlay-2 receiver may still be connecting or have refused the session."
                            >not connected</span
                          >
                        {/if}
                      </span>
                      {#if m.present && isVirtual(m.node_name)}
                        <VolumeControl
                          percent={m.volume == null ? null : Math.round(m.volume * 100)}
                          muted={muted[m.node_name] ?? false}
                          onVolume={(pct) => onVolume(m.node_name, pct)}
                          onMute={() => onMute(m.node_name)}
                        />
                      {/if}
                    </div>
                  {/each}
                </div>
              </div>
            {/each}
          </div>
        </div>
      {/if}
    </div>
  {/if}
</div>

{#if helpOpen}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="modal-backdrop" onclick={() => (helpOpen = false)}>
    <div
      class="modal-card card"
      role="dialog"
      aria-modal="true"
      aria-labelledby="routing-help-title"
      tabindex="-1"
      bind:this={helpEl}
      onclick={(e) => e.stopPropagation()}
    >
      <div class="card-head">
        <h2 id="routing-help-title">Reading the routing graph</h2>
        <button class="ghost" type="button" onclick={() => (helpOpen = false)}>Close</button>
      </div>
      <RoutingHelp {anyLatency} {anyXruns} />
    </div>
  </div>
{/if}

<style>
  /* The preceding sibling on the page is the group grid, not a card, so the
     `.card + .card` rule can't give this one its gap. */
  .graph-card {
    margin-top: 16px;
  }
  /* Disclosure title: the whole heading is the hit target (same chevron idiom as
     the Sources/Outputs cards). */
  .collapse-toggle {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 0;
    background: none;
    border: none;
    border-radius: 6px;
    font: inherit;
    color: inherit;
    cursor: pointer;
  }
  .collapse-toggle:hover {
    box-shadow: none;
    color: var(--primary-color);
  }
  .chevron {
    font-size: 0.7rem;
    line-height: 1;
    color: var(--secondary-text-color);
    transition: transform 0.15s ease;
  }
  .collapse-toggle[aria-expanded='true'] .chevron {
    transform: rotate(90deg);
  }
  /* Collapsed, the card is just its title row. */
  #routing-graph {
    margin-top: 8px;
  }

  /* The per-speaker escape hatch, deliberately understated. */
  .expert {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin: 0;
    font-size: 0.8rem;
    color: var(--secondary-text-color);
    cursor: pointer;
  }
  .expert input {
    width: auto;
    margin: 0;
  }

  .flow {
    overflow-x: auto;
  }
  .canvas {
    position: relative;
    min-width: 580px; /* two 240px columns + link gutter; scrolls below this */
  }
  .canvas.dragging {
    user-select: none;
    cursor: grabbing;
  }
  .wires {
    position: absolute;
    inset: 0;
    z-index: 1;
    pointer-events: none;
    overflow: visible;
  }
  .wire {
    fill: none;
    stroke: var(--primary-color);
    stroke-width: 2.5;
  }
  .wire.off {
    stroke: var(--secondary-text-color);
    stroke-dasharray: 5 5;
    opacity: 0.6;
  }
  /* Only some of the group's speakers are on this source — the mixed state. */
  .wire.partial {
    stroke: var(--warning-color, #f9a825);
    stroke-dasharray: 2 6;
  }
  /* Routed and reachable, but no speaker on it has a live session yet, so nothing
     is being delivered. Solid + dimmed: the route is real (unlike `.off`), it just
     isn't carrying — and it must never animate, or it would claim playback the
     announce arbiter would refuse. */
  .wire.waiting {
    stroke: var(--warning-color, #f9a825);
    opacity: 0.55;
  }
  /* Routed + carrying signal: dashes travel source→speaker. dashoffset step must
     equal one dash+gap period (6+8) so the loop is seamless. */
  .wire.active {
    stroke-dasharray: 6 8;
    animation: wire-flow 0.8s linear infinite;
  }
  @keyframes wire-flow {
    to {
      stroke-dashoffset: -14;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .wire.active {
      animation: none; /* fall back to a solid line */
      stroke-dasharray: none;
    }
  }
  .ghost {
    fill: none;
    stroke: var(--primary-color);
    stroke-width: 2.5;
    stroke-dasharray: 4 4;
    opacity: 0.7;
  }
  /* Transparent fat overlay of each wire so it's easy to click to remove. */
  .hit {
    fill: none;
    stroke: transparent;
    stroke-width: 16;
    pointer-events: stroke;
    cursor: pointer;
  }
  .hit:hover + .wire {
    stroke: var(--error-color, #d33);
  }

  .node {
    position: absolute;
    z-index: 2;
    box-sizing: border-box;
    display: flex;
    align-items: center;
    padding: 6px 10px;
    border: 1px solid color-mix(in srgb, var(--secondary-text-color) 25%, transparent);
    border-radius: 10px;
    background: var(--card-background-color, var(--ha-card-background, #fff));
  }
  .node.src {
    left: 0;
  }
  .node.out {
    right: 0;
    padding: 8px 10px;
  }
  /* A group card reads as a container of its speakers. */
  .node.out.group {
    border-color: color-mix(in srgb, var(--primary-color) 45%, transparent);
    background: color-mix(in srgb, var(--primary-color) 5%, var(--card-background-color, #fff));
  }
  .node.offline {
    opacity: 0.5;
  }
  .body {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
    flex: 1;
  }
  /* Right-column card body: optional group head, then one row per speaker, each
     an exact height so the handle arithmetic above stays true. */
  .tbody {
    display: flex;
    flex-direction: column;
    gap: 6px;
    min-width: 0;
    flex: 1;
    justify-content: center;
  }
  .ghead {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .ghead .nm {
    font-weight: 600;
  }
  .cnt {
    flex: none;
    margin-left: auto;
    font-size: 0.68rem;
    color: var(--secondary-text-color);
    white-space: nowrap;
  }
  .member {
    display: flex;
    flex-direction: column;
    gap: 4px;
    justify-content: center;
    min-width: 0;
    overflow: hidden;
  }
  .member.offline {
    opacity: 0.55;
  }
  .empty-row {
    justify-content: center;
  }
  .nm {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 500;
  }
  .head-name {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    min-width: 0;
  }
  /* Connection handles at the inner edge of each column. */
  .handle {
    position: absolute;
    top: 50%;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--primary-color);
    border: 2px solid var(--card-background-color, #fff);
    transform: translate(-50%, -50%);
    cursor: grab;
    touch-action: none;
  }
  .handle:hover {
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--primary-color) 25%, transparent);
  }
  .handle.right {
    left: 100%;
  }
  .handle.left {
    left: 0;
  }

  .meter {
    flex: 1;
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
  .tag {
    flex: none;
    font-size: 0.65rem;
    font-weight: 600;
    text-transform: uppercase;
    padding: 1px 5px;
    border-radius: 6px;
    letter-spacing: 0.03em;
  }
  .tag.off {
    background: color-mix(in srgb, var(--secondary-text-color) 18%, transparent);
    color: var(--secondary-text-color);
  }
  .tag.grp {
    background: color-mix(in srgb, var(--success-color, #2e7d32) 20%, transparent);
    color: var(--success-color, #2e7d32);
  }
  .tag.mix {
    background: color-mix(in srgb, var(--warning-color, #f9a825) 22%, transparent);
    color: var(--warning-color, #b26a00);
  }
  /* Present, but no session — an expected transient for a pw-sink target, a
     problem if it persists. Amber like `mix`, its own class so the two can
     diverge; not uppercase-cramped since the label is longer. */
  .tag.wait {
    background: color-mix(in srgb, var(--warning-color, #f9a825) 18%, transparent);
    color: var(--warning-color, #b26a00);
    text-transform: none;
    white-space: nowrap;
  }
  /* Estimated per-node buffering (~ms), rendered inline next to the name/meter. */
  .lat {
    flex: none;
    font-size: 0.68rem;
    font-variant-numeric: tabular-nums;
    color: var(--secondary-text-color);
    white-space: nowrap;
  }
  /* Per-node xrun count. Amber = has dropped historically; red + pulse = the
     count is climbing right now (actively dropping). */
  .xrun {
    flex: none;
    font-size: 0.68rem;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    padding: 0 5px;
    border-radius: 6px;
    background: color-mix(in srgb, var(--warning-color, #f9a825) 22%, transparent);
    color: var(--warning-color, #b26a00);
  }
  .xrun.hot {
    background: color-mix(in srgb, var(--error-color, #d33) 22%, transparent);
    color: var(--error-color, #d33);
    animation: xrun-pulse 0.8s ease-in-out infinite;
  }
  @keyframes xrun-pulse {
    50% {
      opacity: 0.45;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .xrun.hot {
      animation: none;
    }
  }
  .x {
    flex: none;
    border: none;
    background: none;
    cursor: pointer;
    color: var(--secondary-text-color);
    font-size: 0.8rem;
    padding: 0 2px;
    line-height: 1;
  }
  .x:hover {
    color: var(--error-color, #d33);
  }
</style>
