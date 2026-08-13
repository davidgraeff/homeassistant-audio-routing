<script lang="ts">
  import { untrack, onDestroy } from 'svelte';
  import { artworkOf, nowPlayingOf, peakOf, routing, xrunsOf } from '../../lib/routing';
  import { api } from '../../lib/api';
  import { run, runUndoable, toast } from '../../lib/toast';
  import { askConfirm, removeOutputConfirm } from '../../lib/confirm.svelte';
  import type { MusicGroup, NowPlaying, RoutingNode } from '../../lib/types';
  import VolumeControl from '../ui/VolumeControl.svelte';
  import { SENDSPIN_DEV_PREFIX, hasAnyLevelControl, levelCaps } from '../../lib/outputs/level';
  import RoutingHelp from './RoutingHelp.svelte';

  // Interactive bipartite routing graph: sources on the left, what they play on
  // on the right, links drawn as SVG curves between them. Drag from a handle to
  // one on the opposite side to route; click a curve to remove that route;
  // Ctrl-drag a handle to lift the wires already on it and drop them on another
  // handle in the same column (move a group's music to a different group).
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

  // Virtual outputs (sendspin + AirPlay-2) carry volume/mute in-band; both are
  // driven entirely by the routing matrix over the WebSocket (no polling). RAOP
  // (AirPlay 1) is being retired and no longer exposes volume in this UI.
  /** Does the daemon have a level it can drive for this node?
   *
   *  The daemon's own answer (`level_caps`), per output and per moment — it is the only
   *  party that knows whether a receiver agent is on the other end. Sources carry no caps,
   *  so this excludes them without naming prefixes, which is what the old `isVirtual`
   *  name-prefix test was really for. Never derived from the kind: enumerating prefixes
   *  here is what hid the control from every pw-sink host the daemon could already drive,
   *  and inferring it from `volume != null` asked "has a level arrived?" instead. */
  const hasLevel = (n: RoutingNode) => hasAnyLevelControl(n);

  /** Expert view: bypass groups and wire individual speakers. */
  let showSpeakers = $state(false);

  // The ~ms estimates, off by default: they repeat on every card and every speaker row,
  // and they only mean something to someone who is actually tuning buffers. Remembered
  // like the card's own open state — whoever wants them wants them every session, and
  // whoever doesn't never turns them on.
  const LAT_KEY = 'par-graph-latency';
  let showLatency = $state(localStorage.getItem(LAT_KEY) === '1');
  $effect(() => {
    localStorage.setItem(LAT_KEY, showLatency ? '1' : '0');
  });

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
  // The wires need a *predictable* amount of room, not everything left over: the
  // cards carry the names, badges and sliders that get elided, so they are what
  // should take the slack. So the link gutter is fixed and the two columns split
  // the rest evenly. It narrows on a small canvas, where 200 px of empty middle
  // costs more than the curves gain from it.
  const LINK_W = 200; // link gutter at full width
  const LINK_W_MIN = 110; // …and on a narrow canvas
  const COL_MIN = 180; // below this the canvas scrolls instead (see .canvas min-width)
  const ROW_SRC = 52; // source card height
  const GAP = 16; // vertical gap between cards
  const TOP = 8; // top padding
  const HIT = 80; // drop hit-test radius (px) around a target handle
  const PAD = 8; // vertical padding inside a target card
  const HEAD_H = 22; // a group's title row
  const MEM_VOL_H = 42; // speaker row carrying a volume slider
  const MEM_H = 22; // speaker row without one
  const MEM_GAP = 6; // gap between speaker rows
  const NP_H = 24; // now-playing row on a source card (see srcLayout)
  const NP_GAP = 4; // gap between a source's name row and its now-playing row

  let canvasEl: HTMLDivElement | undefined = $state();
  let Wc = $state(0); // measured canvas width
  /** Link gutter for the measured width, and the card width that leaves. The gutter
   *  reaches its full 200 px at ~910 px of canvas and gives way below that. */
  const linkW = $derived(Math.max(LINK_W_MIN, Math.min(LINK_W, Wc * 0.22)));
  const colW = $derived(Math.max(COL_MIN, (Wc - linkW) / 2));
  // A drag in progress. `rewire` is empty for the ordinary "draw a new wire"
  // drag; non-empty when the handle was Ctrl-grabbed, in which case it holds the
  // *far* ends of the wires now in hand (target keys when a source handle was
  // grabbed, source names when a target handle was) and the drop lands on the
  // same column instead of the opposite one.
  type Drag = { kind: 'source' | 'target'; name: string; x: number; y: number; rewire: string[] };
  let dragging = $state<Drag | null>(null);
  /** Ctrl/⌘ held: hints which handles can be grabbed to move their wires. */
  let modHeld = $state(false);

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

  /** The full track, for the row's tooltip — the row itself elides at the card width. */
  function npTitle(np: NowPlaying): string {
    const parts = [ np.title, np.artist, np.album ].filter(Boolean);
    const label = parts.join(' — ') || 'playing';
    return np.state === 'paused' ? `${label} (paused)` : label;
  }

  // Must agree with the render gate below, or a row is sized for a control it does
  // not draw (or vice versa).
  const memberH = (n: RoutingNode) => (n.present && hasLevel(n) ? MEM_VOL_H : MEM_H);
  function targetH(t: Target): number {
    const rows = t.members.length ? t.members.map(memberH) : [MEM_H];
    const body = rows.reduce((a, b) => a + b, 0) + (rows.length - 1) * MEM_GAP;
    return 2 * PAD + body + (t.kind === 'group' ? HEAD_H + MEM_GAP : 0);
  }

  // Left column: stacked top-to-bottom like the right one, because a source card
  // grows by one row when that input reports what it is playing (now_playing.rs).
  // The handle stays on the *name* row's center rather than the card's, so adding a
  // track line doesn't drag the wire down and re-draw the whole graph.
  const srcLayout = $derived.by(() => {
    let y = TOP;
    return S.map((n) => {
      const h = ROW_SRC + (nowPlayingOf($routing, n.node_name) ? NP_H + NP_GAP : 0);
      const box = { n, top: y, h, x: colW, y: y + ROW_SRC / 2, name: n.node_name };
      y += h + GAP;
      return box;
    });
  });
  // Handle centers, by node name / target key. Source handles sit on the right
  // edge of the left column; target handles on the left edge of the right one.
  const srcPos = $derived(srcLayout.map(({ name, x, y }) => ({ name, x, y })));
  const outX = $derived(Math.max(colW, Wc - colW));
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

  const srcColH = $derived(srcLayout.reduce((a, b) => a + b.h + GAP, 0));
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

  // Animate a routed wire while its source is carrying signal. Peaks arrive on the
  // `meters` frame (four times a second while anything is audible, nothing at all
  // while the house is silent); we hold the "flowing" state for a short while past
  // the last above-threshold frame so brief quiet passages in music don't make the
  // animation stutter or drop out. Threshold matches when the meter-fill shows.
  const FLOW_THRESH = 0.02; // peak level (0–1) that counts as signal present
  const FLOW_HOLD = 1200; // ms to keep flowing after signal falls below threshold
  let flowing = $state<Record<string, boolean>>({});
  let flowTimers: Record<string, ReturnType<typeof setTimeout>> = {};

  // Depends on the meters slice, NOT on the matrix: the matrix now only changes when
  // the graph does, so tracking it here would freeze the animation between changes.
  $effect(() => {
    const meters = $routing.meters;
    untrack(() => {
      for (const s of $routing.matrix.sources) {
        if (!(s.present && (meters[s.node_name]?.peak ?? 0) > FLOW_THRESH)) continue;
        if (!flowing[s.node_name]) flowing = { ...flowing, [s.node_name]: true };
        clearTimeout(flowTimers[s.node_name]); // extend the hold on each active frame
        flowTimers[s.node_name] = setTimeout(() => {
          flowing = { ...flowing, [s.node_name]: false };
        }, FLOW_HOLD);
      }
    });
  });
  // Peak hold for the input meters: the bar follows the live level, a thin line marks
  // the highest one recently seen and slides back down, so a transient is still
  // visible after it has decayed (a plain bar at 4 Hz just flickers past it).
  //
  // The slide runs on its own timer rather than on incoming frames, because the
  // `meters` frame stops arriving *entirely* while the house is silent (see
  // `peakOf`) — driven by frames, the line would freeze at the last loud moment and
  // stay there. The timer only exists while some line is still above zero.
  const PEAK_FALL = 0.4; // full-scale fraction the line slides per second
  const PEAK_TICK = 100; // ms between slides
  let peakHold = $state<Record<string, number>>({});
  let peakTimer: ReturnType<typeof setInterval> | null = null;

  function slidePeaks() {
    const step = (PEAK_FALL * PEAK_TICK) / 1000;
    const next: Record<string, number> = {};
    let anyLeft = false;
    for (const [name, v] of Object.entries(peakHold)) {
      next[name] = Math.max(0, v - step);
      if (next[name] > 0) anyLeft = true;
    }
    peakHold = next;
    if (anyLeft) return;
    if (peakTimer !== null) clearInterval(peakTimer);
    peakTimer = null;
  }

  // Rising edge only; the slide above owns the way down. Same meters-slice dependency
  // as the wire animation, for the same reason.
  $effect(() => {
    const meters = $routing.meters;
    untrack(() => {
      let next = peakHold;
      let changed = false;
      for (const s of $routing.matrix.sources) {
        const peak = meters[s.node_name]?.peak ?? 0;
        if (peak <= (next[s.node_name] ?? 0)) continue;
        if (!changed) next = { ...peakHold };
        next[s.node_name] = peak;
        changed = true;
      }
      if (!changed) return;
      peakHold = next;
      peakTimer ??= setInterval(slidePeaks, PEAK_TICK);
    });
  });

  onDestroy(() => {
    for (const t of Object.values(flowTimers)) clearTimeout(t);
    for (const t of Object.values(xrunTimers)) clearTimeout(t);
    if (peakTimer !== null) clearInterval(peakTimer);
  });

  /** The far ends of every *drawn* wire on this handle — what a Ctrl-grab picks
   *  up. Read off `edges` so it's exactly what the user sees on the handle. */
  function attached(kind: 'source' | 'target', name: string): string[] {
    return kind === 'source'
      ? edges.filter((e) => e.source === name).map((e) => e.target.key)
      : edges.filter((e) => e.target.key === name).map((e) => e.source);
  }
  /** Handles that have at least one wire, so a Ctrl-grab there does something. */
  const busySources = $derived(new Set(edges.map((e) => e.source)));
  const busyTargets = $derived(new Set(edges.map((e) => e.target.key)));

  // Which column can accept the current drop: the opposite one for a new wire,
  // the same one when wires are being moved.
  const dropSide = $derived.by<'source' | 'target' | null>(() => {
    if (!dragging) return null;
    if (dragging.rewire.length) return dragging.kind;
    return dragging.kind === 'source' ? 'target' : 'source';
  });

  // Wires drawn to the pointer: one for a new link, one per picked-up link while
  // rewiring (each keeps its far end and follows the cursor at this end).
  const ghosts = $derived.by<string[]>(() => {
    const d = dragging;
    if (!d || Wc === 0) return [];
    if (d.rewire.length) {
      return d.rewire
        .map((far) => (d.kind === 'source' ? targetByKey.get(far) : srcByName.get(far)))
        .filter((p): p is NonNullable<typeof p> => !!p)
        .map((p) => (d.kind === 'source' ? bezier(d.x, d.y, p.x, p.y) : bezier(p.x, p.y, d.x, d.y)));
    }
    const o = d.kind === 'source' ? srcByName.get(d.name) : targetByKey.get(d.name);
    return o ? [bezier(o.x, o.y, d.x, d.y)] : [];
  });

  const edgeKey = (source: string, targetKey: string) => `${source} ${targetKey}`;
  /** Edges hidden because their end is currently in hand (a ghost replaces them). */
  const lifted = $derived.by<Set<string>>(() => {
    const d = dragging;
    if (!d?.rewire.length) return new Set<string>();
    return new Set(
      d.kind === 'source' ? d.rewire.map((k) => edgeKey(d.name, k)) : d.rewire.map((s) => edgeKey(s, d.name)),
    );
  });

  function pointerXY(e: PointerEvent): { x: number; y: number } {
    const r = canvasEl?.getBoundingClientRect();
    return r ? { x: e.clientX - r.left, y: e.clientY - r.top } : { x: 0, y: 0 };
  }

  function startDrag(kind: 'source' | 'target', name: string, e: PointerEvent) {
    e.preventDefault();
    // Ctrl/⌘-drag lifts the wires already on this handle instead of drawing a new
    // one. With nothing on it there's nothing to lift, so it stays a plain drag.
    const rewire = e.ctrlKey || e.metaKey ? attached(kind, name) : [];
    dragging = { kind, name, rewire, ...pointerXY(e) };
  }
  function onMove(e: PointerEvent) {
    if (dragging) dragging = { ...dragging, ...pointerXY(e) };
  }

  /** Nearest handle to the drop point, or null if the drop was in open space —
   *  in which case nothing changes and the wires snap back to where they were. */
  function nearest(candidates: { name: string; x: number; y: number }[], x: number, y: number): string | null {
    let best: string | null = null;
    let bestD = HIT * HIT;
    for (const t of candidates) {
      const d = (t.x - x) ** 2 + (t.y - y) ** 2;
      if (d <= bestD) {
        bestD = d;
        best = t.name;
      }
    }
    return best;
  }

  async function onUp(e: PointerEvent) {
    if (!dragging) return;
    const drag = dragging;
    dragging = null;
    const { x, y } = pointerXY(e);
    const sameSide = drag.rewire.length > 0;
    // source column for a source-side drop, target column otherwise.
    const toSources = (drag.kind === 'source') === sameSide;
    const best = nearest(toSources ? srcPos : layout, x, y);
    if (!best) return;
    if (sameSide) {
      if (best !== drag.name) await rewireTo(drag, best);
      return;
    }
    const source = drag.kind === 'source' ? drag.name : best;
    const target = targetByKey.get(drag.kind === 'source' ? best : drag.name)?.t;
    if (!target) return;
    await route(source, target);
  }

  /** Move the wires lifted at one handle onto another handle in the same column:
   *  the new endpoint is routed, then the old link dropped. Both halves go
   *  through the same calls a plain drag makes, so group routing stays
   *  exclusive — which is also why a group destination can only end up on one
   *  source even if several were in hand. */
  async function rewireTo(drag: Drag, to: string) {
    if (drag.kind === 'target') {
      const dst = targetByKey.get(to)?.t;
      const src = targetByKey.get(drag.name)?.t;
      if (!dst || !src) return;
      for (const source of drag.rewire) {
        const old = linkedMembers(src, source); // before routing: the store lags
        if (!(await route(source, dst))) return;
        for (const m of old) {
          if (!(await run(() => api.unlink(source, m.node_name)))) return;
        }
      }
      if (dst.kind === 'group' && drag.rewire.length > 1) {
        toast(
          'info',
          `"${dst.name}" plays one source at a time — it kept ${disp(srcInfo, drag.rewire[drag.rewire.length - 1])}.`,
        );
      }
    } else {
      // The source end moved: everything this source fed now takes `to` instead.
      for (const key of drag.rewire) {
        const t = targetByKey.get(key)?.t;
        if (!t) continue;
        const old = linkedMembers(t, drag.name); // before routing: the store lags
        if (t.kind === 'group') {
          // One call does both halves — every member onto `to`, every other
          // source (the lifted one included) dropped. Not via `route`, which
          // would call it a no-op for a mixed group already carrying `to`.
          if (!(await run(() => api.routeMusicGroup(t.id, to), `"${t.name}" now playing ${disp(srcInfo, to)}`))) return;
        } else {
          if (!(await route(to, t))) return;
          for (const m of old) {
            if (!(await run(() => api.unlink(drag.name, m.node_name)))) return;
          }
        }
      }
    }
  }

  /** Route `source` to a whole target. Groups go through the group endpoint (the
   *  same reconciling call as their Source dropdown: every member on that one
   *  source, any other source removed); a lone speaker takes an extra link, so
   *  two sources can be mixed into one speaker deliberately. Returns whether the
   *  route is in place (including "it already was"). */
  async function route(source: string, target: Target): Promise<boolean> {
    if (target.members.length === 0) {
      // Nothing to link to. Says so rather than accepting the drop and doing
      // nothing — and, moving wires, keeps them where they are instead of
      // dropping them into a group that can't play them.
      toast('info', `"${target.name}" has no speakers yet — add one first.`);
      return false;
    }
    const linked = linkedMembers(target, source);
    if (linked.length === target.members.length) return true; // already routed
    if (target.kind === 'group') {
      return await run(
        () => api.routeMusicGroup(target.id, source),
        `"${target.name}" now playing ${disp(srcInfo, source)}`,
      );
    }
    return await run(() => api.link(source, target.members[0].node_name));
  }

  const disp = (map: Map<string, RoutingNode>, name: string) => map.get(name)?.display_name ?? name;

  // Estimated buffering (ms) a node adds — the configured jitter/playout buffer
  // the daemon reports (routing.rs `latency_ms`), not a measured figure.
  const fmtLat = (ms: number | null | undefined): string | null => (ms == null ? null : `~${ms} ms`);

  // Whether any node carries a latency estimate *and* the badges are on — the help
  // explains a badge, so with them hidden its ~ms paragraph explains nothing on screen.
  const anyLatency = $derived(showLatency && [...S, ...O].some((n) => n.latency_ms != null));

  // Per-node xrun (dropped-cycle) counts from the profiler — pw-top's ERR. The
  // count is cumulative, so a non-zero value alone means "has dropped at some
  // point"; what matters is whether it's climbing *now*. Hold a "hot" flag for a
  // short while after each increase so an actively-stuttering node lights up red
  // (mirrors the wire-flow hold above). Absent from the `meters` frame = never
  // dropped a cycle, profiling off, or a virtual output → no badge.
  const XRUN_HOT_HOLD = 2500; // ms to keep a node flagged after its count rises
  let xrunHot = $state<Record<string, boolean>>({});
  let prevXruns: Record<string, number> = {};
  let xrunTimers: Record<string, ReturnType<typeof setTimeout>> = {};
  const anyXruns = $derived(Object.values($routing.meters).some((m) => (m.xruns ?? 0) > 0));

  // Also driven by the meters slice — counts climb without the graph changing.
  $effect(() => {
    const meters = $routing.meters;
    untrack(() => {
      for (const [name, m] of Object.entries(meters)) {
        if (m.xruns == null) continue;
        const prev = prevXruns[name];
        if (prev != null && m.xruns > prev) {
          if (!xrunHot[name]) xrunHot = { ...xrunHot, [name]: true };
          clearTimeout(xrunTimers[name]);
          xrunTimers[name] = setTimeout(() => {
            xrunHot = { ...xrunHot, [name]: false };
          }, XRUN_HOT_HOLD);
        }
        prevXruns[name] = m.xruns;
      }
    });
  });

  /** Click a wire to remove that route — for a group, from every member on it.
   *
   *  The most-clicked destructive control in the app, and the cheapest to get
   *  wrong: re-drawing the wire is the same drag that made it. So it just does it
   *  and offers an Undo, which relinks exactly the members it unlinked — asking
   *  first taxed every deliberate click to guard a stray one. */
  async function removeEdge(e: Edge) {
    const what = e.target.kind === 'group' ? `group ${e.target.name}` : e.target.name;
    // Captured before the first unlink: `linkedMembers` reads the live matrix, and
    // the undo has to relink exactly the members this click took off.
    const members = linkedMembers(e.target, e.source);
    // A refused call throws, with the daemon's reason — so this only has to stop at the
    // first one rather than inspect a flag.
    const each = async (call: (output: string) => Promise<unknown>) => {
      for (const m of members) {
        await call(m.node_name);
      }
    };
    await runUndoable(
      () => each((o) => api.unlink(e.source, o)),
      `Removed route: ${disp(srcInfo, e.source)} → ${what}`,
      () => each((o) => api.link(e.source, o)),
      'Route restored',
    );
  }

  // Asked in a bubble anchored to the node's own ✕, not in the app's modal: a
  // backdrop over the graph hides the one thing the question is about — which of a
  // dozen look-alike cards this is.
  //
  // Positioned in viewport coordinates, taken from the button at click time, and
  // rendered outside the graph card: the canvas scrolls and is exactly as tall as
  // its computed layout, so a bubble living inside it is clipped the moment it
  // hangs off the bottom card. A scroll therefore dismisses it (see `onKey`'s
  // neighbours on <svelte:window>) rather than leaving it behind.
  let forgetting = $state<{ node: RoutingNode; x: number; y: number } | null>(null);
  const CPOP_W = 250;

  function askForget(node: RoutingNode, btn: HTMLElement) {
    if (forgetting?.node.node_name === node.node_name) {
      forgetting = null;
      return;
    }
    const r = btn.getBoundingClientRect();
    forgetting = {
      node,
      x: Math.max(8, Math.min(r.left - 8, window.innerWidth - CPOP_W - 8)),
      y: Math.min(r.bottom + 8, window.innerHeight - 130),
    };
  }

  async function forget(node: RoutingNode) {
    forgetting = null;
    await run(() => api.forgetEntity(node.node_name), `Forgot '${node.display_name}'`);
  }

  // Same ✕, different meaning on an output: an output is in this matrix because
  // it was *added* on the Outputs page, so merely forgetting its routing would
  // leave the row sitting there and the ✕ would look broken. Removing un-adds it
  // — routing, group membership and Home Assistant entity go with it, and a
  // device that's still on the network reappears as a discovered offer.
  // Same wording as the Outputs page's Remove (`removeOutputConfirm`) — it is the
  // same call on the same thing, and the two used to explain themselves
  // differently.
  async function removeOutput(node: RoutingNode) {
    if (!(await askConfirm(removeOutputConfirm(node.display_name, node.present)))) return;
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
        if (!hasLevel(o)) continue;
        if (typeof o.muted === 'boolean' && mNext[o.node_name] !== o.muted) {
          if (!mChanged) mNext = { ...muted };
          mNext[o.node_name] = o.muted;
          mChanged = true;
        }
      }
      if (mChanged) muted = mNext;
    });
  });

  // Ctrl/⌘ held is a UI hint only (the drag reads the modifier off the pointer
  // event itself); Escape abandons a drag, leaving the wires as they were.
  function onKey(e: KeyboardEvent) {
    modHeld = e.ctrlKey || e.metaKey;
    if (e.type !== 'keydown' || e.key !== 'Escape') return;
    helpOpen = false;
    dragging = null;
    forgetting = null;
  }

  /** A pointer landing anywhere but the bubble (or the ✕ that owns it) puts the
   *  forget question away — the graph's own version of clicking the backdrop. */
  function onGlobalDown(e: PointerEvent) {
    if (!forgetting) return;
    if ((e.target as Element | null)?.closest('.cpop, .x')) return;
    forgetting = null;
  }

  // The endpoint per kind lives in lib/outputs/level.ts — the graph has only a node
  // name, and guessing "AP2 or else sendspin" here is what sent pw-sink hosts' levels
  // to the sendspin endpoint, which stored them for a device that never connects.
  async function onVolume(nodeName: string, pct: number) {
    try {
      await api.setOutputVolume(nodeName, pct / 100);
    } catch (e) {
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
  async function onMute(nodeName: string) {
    const next = !muted[nodeName];
    muted = { ...muted, [nodeName]: next }; // optimistic; matrix confirms
    try {
      await api.setOutputMute(nodeName, next);
    } catch (e) {
      // Put the optimistic flip back: an unreachable host answers 503, and the
      // button must not keep claiming a mute that never landed.
      muted = { ...muted, [nodeName]: !next };
      toast('error', e instanceof Error ? e.message : String(e));
    }
  }
</script>

<svelte:window
  onpointermove={onMove}
  onpointerup={onUp}
  onpointerdown={onGlobalDown}
  onkeydown={onKey}
  onkeyup={onKey}
  onblur={() => (modHeld = false)}
  onscroll={() => (forgetting = null)}
  onresize={() => (forgetting = null)}
/>

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
        <label class="expert" title="The ~ms estimate of the buffering each source and speaker adds (configured, not measured)">
          <input type="checkbox" bind:checked={showLatency} />
          Show latencies
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
                <!-- Hidden while this wire's end is in hand: its ghost stands in. -->
                {#if !lifted.has(edgeKey(e.source, e.target.key))}
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
                {/if}
              {/each}
              {#each ghosts as g, i (i)}<path class="ghost" d={g}></path>{/each}
            </svg>

            {#each srcLayout as box (box.n.node_name)}
              {@const n = box.n}
              {@const np = nowPlayingOf($routing, n.node_name)}
              {@const lvl = Math.min(1, peakOf($routing, n.node_name))}
              {@const hold = Math.min(1, peakHold[n.node_name] ?? 0)}
              <div class="node src" class:offline={!n.present} style="top:{box.top}px; height:{box.h}px; width:{colW}px">
                <!-- The input level, as a full-height bar on the card's outer edge. It
                     used to be a horizontal bar sharing the name row, where a long
                     device name squeezed it to zero width and the card silently lost
                     its level; here the two never compete. Always rendered, including
                     for an absent source (whose level is 0 anyway) — the empty track is
                     what says "routed, nothing coming in", and the card's own dimming
                     covers offline. -->
                <div class="vmeter" style="--lvl:{lvl}; --peak:{hold}" title="input level {Math.round(lvl * 100)}%">
                  <div class="vfill"></div>
                  {#if hold > 0}<div class="vpeak"></div>{/if}
                </div>
                <div class="sbody">
                  <div class="body" style="height:{ROW_SRC - 12}px">
                    <span class="nm" title={n.display_name}>{n.display_name}</span>
                    {#if !n.present}
                      <span class="tag off">offline</span>
                      <button
                        class="x"
                        title="Forget saved routing"
                        aria-expanded={forgetting?.node.node_name === n.node_name}
                        onclick={(e) => askForget(n, e.currentTarget)}>✕</button
                      >
                    {:else}
                      {#if showLatency && fmtLat(n.latency_ms)}
                        <span class="lat" title="Estimated input jitter buffer this source adds">{fmtLat(n.latency_ms)}</span>
                      {/if}
                      {#if (xrunsOf($routing, n.node_name) ?? 0) > 0}
                        <span class="xrun" class:hot={xrunHot[n.node_name]} title="Dropped audio cycles (PipeWire xruns) since this node started — pw-top's ERR. Red = climbing now, i.e. dropping out.">⚠ {xrunsOf($routing, n.node_name)}</span>
                      {/if}
                    {/if}
                  </div>
                  <!-- What this input is playing, when it says (now_playing.rs). Only
                       present for a source with a track, so a silent house shows the
                       same compact cards as before. -->
                  {#if np}
                    {@const art = artworkOf(np)}
                    <div class="np" style="height:{NP_H}px" title={npTitle(np)}>
                      {#if art}
                        <!-- Cover art is best-effort decoration: a broken or slow image
                             must not leave a gap where the title should be. -->
                        <img class="art" src={art} alt="" onerror={(e) => ((e.currentTarget as HTMLImageElement).style.display = 'none')} />
                      {/if}
                      {#if np.state === 'paused'}<span class="np-state" title="Paused">⏸</span>{/if}
                      <span class="np-title">{np.title ?? np.album ?? ''}</span>
                      {#if np.artist}<span class="np-sub">{np.artist}</span>{/if}
                    </div>
                  {/if}
                </div>
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <!-- Pinned to the name row's center, which is where `srcLayout` anchors
                     this card's wires — the CSS default (`top: 50%`) is the *card's*
                     center, so a card carrying a now-playing row put the dot half that
                     row below its own wire. -->
                <div
                  class="handle right"
                  style="top:{ROW_SRC / 2}px"
                  class:candidate={dropSide === 'source'}
                  class:rewirable={modHeld && busySources.has(n.node_name)}
                  role="button"
                  tabindex="-1"
                  aria-label="Drag to a group to route"
                  title={busySources.has(n.node_name)
                    ? 'Drag to a group to route — Ctrl-drag to move this source’s wires to another source'
                    : 'Drag to a group to route'}
                  onpointerdown={(e) => startDrag('source', n.node_name, e)}
                  oncontextmenu={(e) => e.preventDefault()}
                ></div>
              </div>
            {/each}

            {#each layout as box (box.t.key)}
              {@const t = box.t}
              <div class="node out" class:group={t.kind === 'group'} style="top:{box.top}px; height:{box.h}px; width:{colW}px">
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <div
                  class="handle left"
                  class:candidate={dropSide === 'target'}
                  class:rewirable={modHeld && busyTargets.has(t.key)}
                  role="button"
                  tabindex="-1"
                  aria-label="Drag to a source to route"
                  title={busyTargets.has(t.key)
                    ? `Drag to a source to route — Ctrl-drag to move what ${t.kind === 'group' ? 'this group' : 'this speaker'} is playing to another one`
                    : 'Drag to a source to route'}
                  onpointerdown={(e) => startDrag('target', t.key, e)}
                  oncontextmenu={(e) => e.preventDefault()}
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
                        {#if showLatency && fmtLat(m.latency_ms)}
                          <span class="lat" title="Estimated playout buffer this speaker adds (group lead + any per-device delay)">{fmtLat(m.latency_ms)}</span>
                        {/if}
                        {#if (xrunsOf($routing, m.node_name) ?? 0) > 0}
                          <span class="xrun" class:hot={xrunHot[m.node_name]} title="Dropped audio cycles (PipeWire xruns) since this node started — pw-top's ERR. Red = climbing now.">⚠ {xrunsOf($routing, m.node_name)}</span>
                        {/if}
                        <!-- Taken over by an alignment run. It belongs here as much as
                             on the Outputs page: this view draws the wire from the
                             source (the routing intent survives a hold, only the audio
                             is displaced), so without it the graph shows a speaker fed
                             by a live source and playing nothing. Same word and same
                             sentence as the Outputs badge, deliberately. -->
                        {#if m.held}
                          <span
                            class="tag held"
                            title="A speaker-timing measurement has taken this output over. Nothing else plays on it until that finishes, and whatever was routed here comes back afterwards."
                            >held</span
                          >
                        {/if}
                        <!-- A diagnosed fault outranks the generic states below: when
                             the daemon knows *why* nothing is playing, showing
                             "offline"/"not connected" alone is what sent people to the
                             logs. Kept on the offline branch too — a receiver demoted
                             for refusing us is offline *and* has a reason. -->
                        {#if m.last_error}
                          <span class="tag fault" title={m.last_error}>fault</span>
                        {/if}
                        {#if !m.present}
                          <span class="tag off">offline</span>
                          <button class="x" title="Remove this output" onclick={() => removeOutput(m)}>✕</button>
                        {:else if m.streaming === false}
                          <!-- Reachable but nothing attached: distinct from offline,
                               and the reason an announcement here would be refused. -->
                          <span
                            class="tag wait"
                            title={m.last_error ??
                              'On the network, but no session is up — nothing routed here is being played. A PipeWire target has to connect to us (its module-rtp-session initiates the handshake); an AirPlay-2 receiver may still be connecting or have refused the session.'}
                            >not connected</span
                          >
                        {/if}
                      </span>
                      <!-- Capability, not kind, and per knob: the daemon reports which of
                           the two it can drive (`level_caps`), so a host that answers for
                           its volume and not its mute gets the slider without a dead
                           button. Sources carry no caps, so they are excluded here too. -->
                      {#if m.present && hasLevel(m)}
                        <VolumeControl
                          percent={m.volume == null ? null : Math.round(m.volume * 100)}
                          muted={muted[m.node_name] ?? false}
                          canVolume={levelCaps(m).volume}
                          canMute={levelCaps(m).mute}
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

{#if forgetting}
  {@const f = forgetting}
  <!-- The forget question, at the ✕ it came from. Outside the graph card on
       purpose — see `askForget`. -->
  <div class="cpop" style="left:{f.x}px; top:{f.y}px" role="dialog" aria-label="Forget saved routing">
    <p>Forget <strong>{f.node.display_name}</strong>’s saved routing? It’s offline; a real device reappears unrouted.</p>
    <div class="cpop-row">
      <button class="ghost" type="button" onclick={() => (forgetting = null)}>Cancel</button>
      <button class="danger" type="button" onclick={() => forget(f.node)}>Forget</button>
    </div>
  </div>
{/if}

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
    /* Two COL_MIN columns + the narrow LINK_W_MIN gutter — the point at which the
       cards stop shrinking and the canvas scrolls instead. Keep in step with those
       two constants. */
    min-width: 470px;
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
  /* Hovering the (invisible, fatter) hit path colors the wire it belongs to: the
     click removes the route immediately now, so the affordance has to say so
     before the click, not after. Sibling selector — `.hit` is emitted directly
     before its `.wire`. */
  .hit:hover + .wire {
    stroke: var(--error-color, #d33);
    opacity: 1;
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
  /* Transparent fat overlay of each wire so it's easy to click to remove. The
     cursor is scissors, because that's the only thing clicking a wire does — a
     plain pointer would promise a selection that doesn't exist. Drawn twice, a
     white halo under a red blade, so it reads on either theme; hotspot sits on
     the blade crossing (12,12). Falls back to `pointer` where SVG cursors are
     refused. */
  .hit {
    fill: none;
    stroke: transparent;
    stroke-width: 16;
    pointer-events: stroke;
    cursor:
      url("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='24' height='24' viewBox='0 0 24 24' fill='none' stroke-width='2' stroke-linecap='round'><g stroke='%23fff' stroke-width='4.5'><circle cx='6' cy='6' r='3'/><circle cx='6' cy='18' r='3'/><path d='M20 4 8.12 15.88M14.47 14.48 20 20M8.12 8.12 12 12'/></g><g stroke='%23d33'><circle cx='6' cy='6' r='3'/><circle cx='6' cy='18' r='3'/><path d='M20 4 8.12 15.88M14.47 14.48 20 20M8.12 8.12 12 12'/></g></svg>")
        12 12,
      pointer;
  }
  .hit:hover + .wire {
    stroke: var(--error-color, #d33);
  }
  /* Mid-drag the wires aren't click targets: no scissors over them, and the
     pointerup that ends the drag can't be read as a click on one. */
  .canvas.dragging .hit {
    pointer-events: none;
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
  /* Only the right padding survives: the input meter is the first child and has to sit
     flush against the left border on all three sides. Dropping the vertical padding
     doesn't move anything — `.sbody` centres the rows in whatever height it gets, and
     it was symmetric, so the name row's centre (and with it every wire anchor) stays
     exactly where `srcLayout` puts it. */
  .node.src {
    left: 0;
    align-items: stretch;
    padding: 0 10px 0 0;
  }
  /* The name row has no filler element any more (the meter used to be it), so push
     everything after the name — latency, xruns, the offline badge — to the far edge. */
  .node.src .body > .nm {
    margin-right: auto;
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
  /* A source card is a column: the name/meter row, then the now-playing row when
     that input reports one. Heights are exact so srcLayout's arithmetic (and with
     it every wire anchor) stays true without measuring the DOM. */
  .sbody {
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 4px;
    min-width: 0;
    flex: 1;
  }
  .sbody > .body {
    flex: none;
  }
  .np {
    display: flex;
    align-items: center;
    gap: 5px;
    min-width: 0;
    font-size: 0.72rem;
    color: var(--secondary-text-color);
    /* One line, always: the card height is computed, so wrapping would push the
       content out of its own box. */
    overflow: hidden;
    white-space: nowrap;
  }
  .np .art {
    flex: none;
    width: 20px;
    height: 20px;
    border-radius: 3px;
    object-fit: cover;
    background: color-mix(in srgb, var(--secondary-text-color) 15%, transparent);
  }
  .np-state {
    flex: none;
  }
  /* The title has priority for the card's width: it shrinks only after the
     artist has given up everything it can, and the full string is in the row's
     tooltip either way. */
  .np-title {
    flex: 0 1 auto;
    min-width: 3.5em;
    overflow: hidden;
    text-overflow: ellipsis;
    color: var(--primary-text-color);
  }
  .np-sub {
    flex: 0 100 auto;
    min-width: 0;
    max-width: 45%;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .np-sub::before {
    content: '·';
    margin-right: 5px;
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
    /* One slider per speaker means a dozen red track-ends on one screen, which
       reads as decoration and drowns out the badges that *are* status. Damped
       towards the track colour: still visibly the danger zone, no longer the
       loudest thing in the graph. The Outputs page keeps the full red. */
    --vol-danger: color-mix(in srgb, var(--error-color) 30%, var(--divider-color));
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
  /* Connection handles at the inner edge of each column. `top: 50%` is right for a
     target card (its wires anchor at the card's center); a source card overrides it
     inline, since its wires anchor on the name row. */
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
  /* Ctrl/⌘ held: this handle carries wires, so grabbing it moves them instead of
     drawing a new one. */
  .handle.rewirable {
    cursor: move;
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--primary-color) 35%, transparent);
  }
  /* Where whatever is in hand can be dropped. */
  .handle.candidate {
    box-shadow: 0 0 0 5px color-mix(in srgb, var(--primary-color) 22%, transparent);
  }
  .handle.right {
    left: 100%;
  }
  .handle.left {
    left: 0;
  }

  /* Input level: a full-height bar on the card's outer edge. Blue, like everything else
     in the graph — as green it was the one status-coloured thing in here that wasn't
     reporting a status. 24px with a gradient that follows the level, over flat,
     segmented and warning-topped bars at 20 and 30px: the alternatives were either
     invisible down a column of cards or graphic enough to out-shout the badges.
     (`spikes/vertical-meter.html` is where those were compared; this comment is its
     answer, so the file can go whenever it stops earning its keep.)

     It clips ITSELF rather than letting the card do it with `overflow: hidden`: the
     wire handle sits at `left: 100%` with half of it outside the card on purpose, and
     clipping at the card slices that dot into a half-disc. 9px is the card's inner
     radius (10px, less its 1px border), so this corner sits exactly inside that one. */
  .vmeter {
    position: relative;
    flex: none;
    width: 24px;
    margin-right: 10px;
    overflow: hidden;
    border-radius: 9px 0 0 9px;
    background: color-mix(in srgb, var(--secondary-text-color) 18%, transparent);
  }
  .vfill {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    height: calc(var(--lvl) * 100%);
    background: linear-gradient(
      to top,
      color-mix(in srgb, var(--primary-color) 45%, transparent),
      var(--primary-color)
    );
    transition: height 120ms linear;
  }
  /* The held peak. Only rendered while it is above zero (see the markup), so a silent
     card is a plain empty track rather than a bar with a line pinned to its floor. */
  .vpeak {
    position: absolute;
    left: 0;
    right: 0;
    bottom: calc(var(--peak) * 100%);
    height: 2px;
    background: color-mix(in srgb, var(--primary-text-color) 45%, transparent);
    transition: bottom 100ms linear;
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
  /* Held by an alignment run: amber like `wait`, since the speaker is fine and simply
     isn't carrying what the wire suggests. Its own class so it reads as a state rather
     than a warning, and `cursor: help` because the sentence is in the tooltip. */
  .tag.held {
    background: color-mix(in srgb, var(--warning-color, #f9a825) 18%, transparent);
    color: var(--warning-color, #b26a00);
    cursor: help;
  }
  /* A named, diagnosed fault — red rather than the amber of "not up yet", because
     this one will not fix itself by waiting. Hover carries the sentence. */
  .tag.fault {
    background: color-mix(in srgb, var(--error-color, #db4437) 18%, transparent);
    color: var(--error-color, #db4437);
    cursor: help;
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
  .x:hover,
  .x[aria-expanded='true'] {
    color: var(--error-color, #d33);
  }

  /* The confirm bubble: a modal over the graph would hide which card is being
     asked about, so the question hangs off that card's ✕ instead. Fixed, and
     rendered outside the graph — the canvas clips (it scrolls, and its height is
     exactly its computed layout), and the card it points at is dimmed when
     offline, which a child could not undo. */
  .cpop {
    position: fixed;
    width: 250px;
    z-index: 60;
    box-sizing: border-box;
    padding: 10px 12px;
    border: 1px solid var(--ha-card-border-color, var(--divider-color));
    border-radius: 10px;
    background: var(--card-background-color, #fff);
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.25);
  }
  .cpop p {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.4;
    color: var(--secondary-text-color);
  }
  .cpop strong {
    color: var(--primary-text-color);
  }
  .cpop-row {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
    margin-top: 10px;
  }
  .cpop-row button {
    padding: 4px 10px;
    font-size: 0.78rem;
  }
</style>
