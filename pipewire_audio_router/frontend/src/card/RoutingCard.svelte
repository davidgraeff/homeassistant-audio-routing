<script lang="ts">
  import type { RoutingModel } from './model.svelte';
  import { DEFAULT_TITLE, showHintOf, showTitleOf, type CardGroup, type CardNode } from './types';

  // The whole house's routing in one picture: inputs on the left, where they play
  // on the right, a wire per route. Nothing else — no volumes, no levels, no
  // now-playing, no per-speaker rows. Those all live in the add-on's own UI; this
  // card answers "what is playing where, and let me change it" at a glance, from a
  // phone, on a dashboard.
  //
  // Rerouting is tap-then-tap rather than drag-a-wire (which is what the add-on's
  // FlowGraph does): a dashboard card is used on touch far more than with a mouse,
  // and a drag between two 44 px targets on a phone is a coin flip. Tap an input to
  // pick it up, then tap targets to toggle them; tap a wire to remove that route.

  interface Props {
    model: RoutingModel;
  }
  let { model }: Props = $props();

  // Fixed geometry: every row is exactly ROW tall, so handle positions are pure
  // arithmetic and no DOM measurement is needed beyond the canvas width.
  const TOP = 6; // padding above the first row
  const ROW = 44; // row height (also the minimum comfortable tap target)
  const GAP = 8; // vertical gap between rows
  const COL_MIN = 84; // column narrow enough for a phone in portrait
  const COL_MAX = 220;
  const COL_SHARE = 0.36; // of the card width, so the wires get a real gutter
  const MID_MIN = 56; // pixels kept clear between the columns for the wires

  let canvasEl = $state<HTMLDivElement>();
  let W = $state(0); // measured canvas width

  const snapshot = $derived(model.snapshot);
  const links = $derived(snapshot.links);
  const sources = $derived(snapshot.sources);
  const presets = $derived(snapshot.presets);
  /** The dropdown's value is a *name*, since that is what it offers; `null` while
   *  the active preset is unknown, which shows an em dash rather than pretending
   *  the first one is on. */
  const activePresetName = $derived(presets.find((p) => p.id === snapshot.active_preset)?.name ?? null);

  /** A routing destination: a whole music group, or a single output that isn't in
   *  one. Groups route as a unit — the same exclusive call as their Source
   *  dropdown — which is why the card can never produce a group whose speakers
   *  disagree about what they play. */
  type Target =
    | { kind: 'group'; key: string; id: string; name: string; members: string[] }
    | { kind: 'solo'; key: string; name: string; members: string[]; node: CardNode };

  const outByName = $derived(new Map(snapshot.outputs.map((o) => [o.node_name, o])));
  const srcByName = $derived(new Map(sources.map((s) => [s.node_name, s])));

  const targets = $derived.by<Target[]>(() => {
    const grouped = new Set(snapshot.groups.flatMap((g) => g.members));
    return [
      ...snapshot.groups.map(
        (g: CardGroup): Target => ({ kind: 'group', key: `g:${g.id}`, id: g.id, name: g.name, members: g.members }),
      ),
      ...snapshot.outputs
        .filter((o) => !grouped.has(o.node_name))
        .map((o): Target => ({ kind: 'solo', key: `o:${o.node_name}`, name: o.display_name, members: [o.node_name], node: o })),
    ];
  });

  /** A target is present when at least one member is in the live graph. A group
   *  with every speaker away is drawn grayed, exactly like a lone absent one. */
  const present = (t: Target) => t.members.some((m) => outByName.get(m)?.present ?? false);

  /** Which of a target's members carry `source`. */
  const linkedMembers = (t: Target, source: string) =>
    t.members.filter((m) => links.some((l) => l.source === source && l.output === m));

  /** Sources feeding any member of `t`. */
  const sourcesOn = (t: Target) =>
    [...new Set(links.filter((l) => t.members.includes(l.output)).map((l) => l.source))];

  // Columns take a *share* of the width rather than all they can get: wires are
  // how the card is read, and at 220 px each they were reduced to stubs in the
  // 56 px left over. COL_MIN still wins on a phone, where a legible name matters
  // more than a long wire.
  const colW = $derived(
    Math.max(COL_MIN, Math.min(COL_MAX, Math.round(W * COL_SHARE), Math.floor((W - MID_MIN) / 2))),
  );
  const rightX = $derived(Math.max(colW, W - colW));
  const yOf = (i: number) => TOP + i * (ROW + GAP) + ROW / 2;
  const colH = (n: number) => (n === 0 ? 0 : TOP * 2 + n * ROW + (n - 1) * GAP);
  const canvasH = $derived(Math.max(ROW + TOP * 2, colH(sources.length), colH(targets.length)));

  const srcY = $derived(new Map(sources.map((s, i) => [s.node_name, yOf(i)])));
  const tgtY = $derived(new Map(targets.map((t, i) => [t.key, yOf(i)])));

  function bezier(x1: number, y1: number, x2: number, y2: number): string {
    const dx = Math.max(28, (x2 - x1) * 0.45);
    return `M${x1},${y1} C${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`;
  }

  /** One drawn wire per (source, target) pair that has at least one link.
   *  `partial` = only some of a group's members are on it, which is the honest
   *  rendering of a group the add-on UI (or an automation) left mixed. */
  type Edge = { source: string; target: Target; partial: boolean; off: boolean; path: string };
  const edges = $derived.by<Edge[]>(() => {
    if (W === 0) return [];
    const out: Edge[] = [];
    for (const t of targets) {
      for (const source of sourcesOn(t)) {
        const y1 = srcY.get(source);
        const y2 = tgtY.get(t.key);
        if (y1 === undefined || y2 === undefined) continue; // link to a vanished node
        const linked = linkedMembers(t, source);
        out.push({
          source,
          target: t,
          partial: linked.length !== t.members.length,
          off: !(srcByName.get(source)?.present ?? false) || !linked.some((m) => outByName.get(m)?.present),
          path: bezier(colW, y1, rightX, y2),
        });
      }
    }
    return out;
  });

  // ---- selection ----------------------------------------------------------
  // Exactly one end may be held at a time. Holding an input and tapping targets
  // is the common direction; holding a target and tapping inputs answers the
  // other question ("what should this room play?") with the same two taps.
  type Held = { kind: 'source'; name: string } | { kind: 'target'; key: string } | null;
  let held = $state<Held>(null);

  const heldTarget = $derived.by(() => {
    const h = held;
    return h?.kind === 'target' ? targets.find((t) => t.key === h.key) : undefined;
  });

  // A held end that disappears from the graph must not stay held — the next tap
  // would route against a node that is no longer there.
  $effect(() => {
    const h = held;
    if (h?.kind === 'source' && !srcByName.has(h.name)) held = null;
    if (h?.kind === 'target' && !targets.some((t) => t.key === h.key)) held = null;
  });

  async function tapSource(name: string) {
    if (held?.kind === 'source') {
      held = held.name === name ? null : { kind: 'source', name };
      return;
    }
    if (heldTarget) {
      const target = heldTarget;
      held = null;
      await toggle(name, target);
      return;
    }
    held = { kind: 'source', name };
  }

  async function tapTarget(t: Target) {
    if (held?.kind === 'target') {
      held = held.key === t.key ? null : { kind: 'target', key: t.key };
      return;
    }
    if (held?.kind === 'source') {
      const source = held.name;
      held = null;
      await toggle(source, t);
      return;
    }
    held = { kind: 'target', key: t.key };
  }

  /** Tapping a target while holding an input toggles that route: fully routed ⇒
   *  remove, anything else (nothing, or a partial/mixed group) ⇒ route it whole.
   *  Sending a partially-routed group to "fully routed" rather than to "off" is
   *  deliberate — the visible state is a wire that isn't all there, so the tap
   *  completes it. */
  async function toggle(source: string, t: Target) {
    if (t.members.length === 0) {
      // A group with no speakers yet. Says so, rather than sending a route the
      // daemon can only reject with a message about a group id.
      model.error = `“${t.name}” has no speakers yet — add one in the add-on first.`;
      return;
    }
    const linked = linkedMembers(t, source);
    if (t.kind === 'group') {
      if (linked.length === t.members.length && t.members.length > 0) await model.unrouteGroup(t.id);
      else await model.routeGroup(t.id, source);
      return;
    }
    if (linked.length) await model.unlink(source, t.members[0]);
    else await model.link(source, t.members[0]);
  }

  /** Tap a wire to remove that route. No confirmation: redrawing it is the same
   *  two taps that made it. */
  async function removeEdge(e: Edge) {
    if (e.target.kind === 'group') {
      // Only this source comes off. A mixed group keeps the rest of its wires,
      // which is why this isn't `unrouteGroup` when several sources are on it.
      for (const member of linkedMembers(e.target, e.source)) {
        await model.unlink(e.source, member);
      }
      return;
    }
    await model.unlink(e.source, e.target.members[0]);
  }

  const wireLabel = (e: Edge) =>
    `Remove route ${srcByName.get(e.source)?.display_name ?? e.source} → ${e.target.name}`;

  function subtitle(t: Target): string {
    if (t.kind === 'solo') return present(t) ? '' : 'offline';
    if (t.members.length === 0) return 'no speakers';
    const parts = [`${t.members.length} speaker${t.members.length === 1 ? '' : 's'}`];
    if (!present(t) && t.members.length) parts.push('offline');
    else if (sourcesOn(t).length > 1) parts.push('mixed');
    return parts.join(' · ');
  }

  /** What the footer says, so the interaction never has to be guessed. */
  const hint = $derived.by(() => {
    if (held?.kind === 'source') {
      const name = srcByName.get(held.name)?.display_name ?? held.name;
      return `Tap where “${name}” should play — or tap it again to cancel.`;
    }
    if (heldTarget) return `Tap the input “${heldTarget.name}” should play — or tap it again to cancel.`;
    return 'Tap an input, then where it should play. Tap a wire to remove a route.';
  });

  const isHeldSource = (name: string) => held?.kind === 'source' && held.name === name;
  const isHeldTarget = (key: string) => held?.kind === 'target' && held.key === key;
  /** A wire touching the held end — highlighted so the consequence of the next
   *  tap is visible before it happens. */
  const isLive = (e: Edge) => isHeldSource(e.source) || isHeldTarget(e.target.key);

  $effect(() => {
    const el = canvasEl;
    if (!el) return;
    // Measured here as well as observed: a ResizeObserver only calls back at the
    // end of a rendering task, so waiting for it would paint one frame of nodes
    // with no wires between them — and with no width there is nothing to draw a
    // wire *to*, so that frame reads as "nothing is routed".
    W = el.clientWidth;
    const ro = new ResizeObserver(([entry]) => {
      W = Math.round(entry.contentRect.width);
    });
    ro.observe(el);
    return () => ro.disconnect();
  });
</script>

<ha-card>
  {#if showTitleOf(model.config)}
    <h1 class="header">{model.config.title || DEFAULT_TITLE}</h1>
  {/if}
  <div class="body">
    {#if model.error}
      <p class="error">{model.error}</p>
    {/if}
    <!-- The preset picker: the whole grouping of the house in one control, which is
         the one thing about routing that is worth a dashboard and cannot be
         expressed as a wire. Only drawn when there is a *choice* — the integration
         sends no presets unless the user works with them, and a lone `Default` is
         nothing to pick. -->
    {#if presets.length > 1}
      <label class="presetrow">
        <span>Preset</span>
        <select
          value={activePresetName ?? ''}
          disabled={model.busy}
          onchange={(e) => void model.setPreset(e.currentTarget.value)}
        >
          {#if activePresetName === null}
            <option value="" disabled>—</option>
          {/if}
          {#each presets as p (p.id)}
            <option value={p.name}>{p.name}</option>
          {/each}
        </select>
      </label>
    {/if}
    {#if !model.loaded}
      <p class="muted">Loading routing…</p>
    {:else if sources.length === 0 && targets.length === 0}
      <p class="muted">No inputs or outputs yet. Add them in the PipeWire Audio Router add-on.</p>
    {:else}
      <div class="canvas" bind:this={canvasEl} style:height="{canvasH}px" class:busy={model.busy}>
        <svg class="wires" width={W} height={canvasH} aria-hidden={edges.length === 0}>
          {#each edges as e (e.source + ' ' + e.target.key)}
            <path class="wire" class:off={e.off} class:partial={e.partial} class:live={isLive(e)} d={e.path} />
            <path
              class="hit"
              d={e.path}
              role="button"
              tabindex="0"
              aria-label={wireLabel(e)}
              onclick={() => removeEdge(e)}
              onkeydown={(ev) => {
                if (ev.key === 'Enter' || ev.key === ' ') {
                  ev.preventDefault();
                  removeEdge(e);
                }
              }}
            />
          {/each}
        </svg>

        <div class="col left" style:width="{colW}px">
          {#each sources as s (s.node_name)}
            <button
              class="node"
              class:absent={!s.present}
              class:held={isHeldSource(s.node_name)}
              style:height="{ROW}px"
              aria-pressed={isHeldSource(s.node_name)}
              onclick={() => tapSource(s.node_name)}
            >
              <span class="name">{s.display_name}</span>
              {#if !s.present}<span class="sub">offline</span>{/if}
              <span class="dot right-dot"></span>
            </button>
          {/each}
        </div>

        <div class="col right" style:width="{colW}px">
          {#each targets as t (t.key)}
            <button
              class="node target"
              class:absent={!present(t)}
              class:held={isHeldTarget(t.key)}
              style:height="{ROW}px"
              aria-pressed={isHeldTarget(t.key)}
              onclick={() => tapTarget(t)}
            >
              <span class="dot left-dot"></span>
              <span class="name">{t.name}</span>
              {#if subtitle(t)}<span class="sub">{subtitle(t)}</span>{/if}
            </button>
          {/each}
        </div>
      </div>
      {#if showHintOf(model.config) || held}
        <!-- Turned off, the hint still comes back while an end is held: that line
             is the only thing saying what the next tap will do, and how to cancel. -->
        <p class="hint">{hint}</p>
      {/if}
    {/if}
  </div>
</ha-card>

<style>
  /* Everything is expressed in Home Assistant's own theme variables, so the card
     follows the dashboard's theme (including dark mode) with no logic of ours. */
  .header {
    font-family: var(--ha-card-header-font-family, inherit);
    font-size: var(--ha-card-header-font-size, 24px);
    font-weight: normal;
    color: var(--ha-card-header-color, var(--primary-text-color));
    padding: 12px 16px 4px;
    margin: 0;
    letter-spacing: -0.012em;
    line-height: 1.2;
  }
  .body {
    padding: 8px 12px 12px;
  }
  .muted,
  .hint {
    color: var(--secondary-text-color);
    font-size: 12px;
    margin: 8px 4px 0;
  }
  .error {
    color: var(--error-color, #db4437);
    font-size: 13px;
    margin: 0 4px 8px;
  }
  /* One row above the graph, deliberately quiet: it changes everything below it,
     so it reads as a mode, not as an action. */
  .presetrow {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 0 4px 10px;
    font-size: 13px;
    color: var(--secondary-text-color);
  }
  .presetrow select {
    flex: 1 1 auto;
    min-width: 0;
    font: inherit;
    color: var(--primary-text-color);
    background: var(--card-background-color, transparent);
    border: 1px solid var(--divider-color, rgba(127, 127, 127, 0.35));
    border-radius: 8px;
    padding: 5px 8px;
  }
  .canvas {
    position: relative;
    width: 100%;
  }
  .canvas.busy {
    /* A call is in flight; the daemon's push is what ends it. Non-interactive
       rather than spinner-ed, so the picture never jumps. */
    pointer-events: none;
    opacity: 0.65;
  }
  .wires {
    position: absolute;
    inset: 0;
    overflow: visible;
  }
  .wire {
    fill: none;
    stroke: var(--primary-color, #03a9f4);
    stroke-width: 2.5;
    stroke-linecap: round;
    pointer-events: none;
  }
  .wire.partial {
    stroke-dasharray: 6 5;
  }
  .wire.off {
    stroke: var(--disabled-text-color, #bdbdbd);
  }
  .wire.live {
    stroke-width: 4;
  }
  .hit {
    fill: none;
    stroke: transparent;
    stroke-width: 16;
    cursor: pointer;
  }
  .hit:focus-visible {
    outline: none;
    stroke: var(--primary-color, #03a9f4);
    stroke-opacity: 0.3;
  }
  .col {
    position: absolute;
    top: 0;
    display: flex;
    flex-direction: column;
    gap: 8px; /* GAP */
    padding-top: 6px; /* TOP */
  }
  .left {
    left: 0;
  }
  .right {
    right: 0;
  }
  .node {
    position: relative;
    box-sizing: border-box;
    display: flex;
    flex-direction: column;
    justify-content: center;
    gap: 1px;
    width: 100%;
    padding: 4px 10px;
    border: 1px solid var(--divider-color, #e0e0e0);
    border-radius: 10px;
    background: var(--card-background-color, #fff);
    color: var(--primary-text-color);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .node.target {
    text-align: right;
    align-items: flex-end;
  }
  .node:hover {
    border-color: var(--primary-color, #03a9f4);
  }
  .node.held {
    border-color: var(--primary-color, #03a9f4);
    box-shadow: 0 0 0 1px var(--primary-color, #03a9f4);
  }
  .node.absent {
    color: var(--disabled-text-color, #bdbdbd);
  }
  .name {
    font-size: 14px;
    line-height: 1.2;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 100%;
  }
  .sub {
    font-size: 11px;
    color: var(--secondary-text-color);
    line-height: 1.1;
  }
  /* The wire's anchor, drawn on the edge the wires leave from so a route visibly
     starts at the row rather than floating beside it. */
  .dot {
    position: absolute;
    top: 50%;
    width: 8px;
    height: 8px;
    margin-top: -4px;
    border-radius: 50%;
    background: var(--divider-color, #e0e0e0);
  }
  .right-dot {
    right: -4px;
  }
  .left-dot {
    left: -4px;
  }
  .node.held .dot,
  .node:hover .dot {
    background: var(--primary-color, #03a9f4);
  }
</style>
