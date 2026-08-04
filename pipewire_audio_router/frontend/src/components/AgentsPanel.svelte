<script lang="ts">
  // Receiver hosts (docs/receiver-agent-plan.md §8). A remote PipeWire host is a
  // pw-sink output only once the `pwrouter-agent` running on it has paired, so
  // this panel sits *above* Discovered on the Outputs tab: approving here is what
  // makes a host appear there.
  //
  // Two lists, deliberately in this order: requests waiting for a decision first
  // (they are the only thing here that needs action), then the hosts already
  // paired. Each request shows the pairing **code** the agent also writes to its
  // own log — approving a request you cannot identify is how you'd hand control of
  // your audio to whoever else is on the network, so the code is the check.
  //
  // Liveness rides the **routing WebSocket**, not a timer of its own. The daemon
  // pokes its change notifier on every pairing event (pwsink_agent.rs) exactly as
  // it does for discovery, and that notifier is what pushes a matrix frame — so a
  // frame arriving is the signal to re-read this list. Same channel the rest of
  // the page already listens on; a separate poll was both redundant and the
  // source of a visible flicker.
  import { onMount, untrack } from 'svelte';
  import { api } from '../lib/api';
  import { routing } from '../lib/routing';
  import { run } from '../lib/toast';
  import type { AgentInfo } from '../lib/types';

  // The parent refreshes its own listings after a pairing decision: an approval
  // turns into a discovered output, a removal takes one away.
  let { onchange }: { onchange?: () => void } = $props();

  let agents = $state<AgentInfo[]>([]);
  // Only ever true before the *first* answer. A poll must not put the card back
  // into a loading state: with no hosts paired yet that made it alternate between
  // “Loading…” and the empty text on every tick, which read as the page flickering.
  let firstLoad = $state(true);
  let busy = $state<Record<string, boolean>>({});

  const pending = $derived(agents.filter((a) => !a.paired));
  const paired = $derived(agents.filter((a) => a.paired));

  // Compared against the last payload so an unchanged poll — the normal case —
  // doesn't reassign `agents` at all. Without this, every tick invalidated the
  // derived lists and re-rendered the section for no reason.
  let lastPayload = '';
  // Polls can outlive their interval on a busy Pi; overlapping them would let an
  // older answer land last. A refresh asked for while one is in flight is queued
  // rather than dropped, so a click that changes something is never left showing
  // stale rows until the next tick.
  let inFlight = false;
  let queued = false;

  /** Adopts a list, from either the mount fetch or a pushed frame. Identical
   * payloads are compared away so a re-send costs no re-render. */
  function apply(next: AgentInfo[]) {
    const payload = JSON.stringify(next);
    if (payload !== lastPayload) {
      lastPayload = payload;
      agents = next;
    }
    firstLoad = false;
  }

  /** First paint only — afterwards the socket pushes (see the effect below). */
  async function refresh(): Promise<void> {
    if (inFlight) {
      queued = true;
      return;
    }
    inFlight = true;
    try {
      apply(await api.agents().catch(() => [] as AgentInfo[]));
    } finally {
      inFlight = false;
      firstLoad = false;
    }
    if (queued) {
      queued = false;
      await refresh();
    }
  }
  onMount(refresh);

  // The daemon pushes this list on the routing socket whenever it changes — a
  // pairing request arriving, a host connecting, a host reporting a new level — so
  // there is nothing to poll and nothing to re-fetch. Note it must *not* react to
  // the matrix frame, which also arrives every 250 ms for the live meters.
  $effect(() => {
    const pushed = $routing.agents;
    untrack(() => {
      if (pushed) apply(pushed);
    });
  });

  async function decide(a: AgentInfo, action: 'approve' | 'deny' | 'forget') {
    busy = { ...busy, [a.identity]: true };
    const label = { approve: 'Pair', deny: 'Decline', forget: 'Remove' }[action];
    const call = {
      approve: () => api.approveAgent(a.identity),
      deny: () => api.denyAgent(a.identity),
      forget: () => api.forgetAgent(a.identity),
    }[action];
    try {
      if (await run(call, `${label} '${a.label}'`)) {
        await refresh();
        onchange?.();
      }
    } finally {
      busy = { ...busy, [a.identity]: false };
    }
  }

  /** What the host is doing right now, as it reports itself. */
  function statusText(a: AgentInfo): string {
    if (!a.connected) return 'Agent offline';
    if (!a.state?.receiving) return 'Connected, not receiving';
    const bits = [`Playing into ${a.state.sink_name ?? 'its default sink'}`];
    if (a.state.volume != null) bits.push(`${Math.round(a.state.volume * 100)}%`);
    if (a.state.muted) bits.push('muted');
    if (a.state.ducked) bits.push('ducked for an announcement');
    return bits.join(' · ');
  }
</script>

<div class="section-head">
  <h3>
    Receiver hosts
    {#if pending.length}<span class="count">{pending.length}</span>{/if}
  </h3>
  {#if paired.length}
    <span class="hint">{paired.filter((a) => a.connected).length} of {paired.length} online</span>
  {/if}
</div>

{#if firstLoad && agents.length === 0}
  <div class="card"><p class="empty" style="padding:0">Loading…</p></div>
{:else if agents.length === 0}
  <div class="card">
    <p class="empty" style="padding:0">
      No receiver hosts yet — see <em>Explain receiver hosts</em> above to set one up.
    </p>
  </div>
{/if}

{#each pending as a (a.identity)}
  <article class="card agent pending">
    <div class="agent-main">
      <h4>{a.label}</h4>
      <p class="agent-sub">
        Wants to pair. Only approve if this code matches the one that machine's agent printed.
      </p>
    </div>
    <div class="agent-code" title="Pairing code — also printed by the agent">{a.code ?? '—'}</div>
    <div class="agent-actions">
      <button class="primary" disabled={busy[a.identity]} onclick={() => decide(a, 'approve')}>
        {busy[a.identity] ? 'Pairing…' : 'Pair'}
      </button>
      <button disabled={busy[a.identity]} onclick={() => decide(a, 'deny')}>Decline</button>
    </div>
  </article>
{/each}

{#each paired as a (a.identity)}
  <article class="card agent" class:offline={!a.connected}>
    <div class="agent-main">
      <h4>{a.label}</h4>
      <p class="agent-sub">
        {statusText(a)}
        {#if a.node_name}<span class="node">{a.node_name}</span>{/if}
      </p>
    </div>
    <div class="agent-actions">
      <button class="danger" disabled={busy[a.identity]} onclick={() => decide(a, 'forget')}>
        {busy[a.identity] ? 'Removing…' : 'Remove'}
      </button>
    </div>
  </article>
{/each}

<style>
  /* Mirrors the Outputs tab's own section/card look rather than introducing a
     second style for the same page. */
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
  .hint {
    font-size: 0.8rem;
    color: var(--secondary-text-color);
  }
  .agent {
    display: flex;
    align-items: center;
    gap: 16px;
    flex-wrap: wrap;
  }
  .agent.offline {
    opacity: 0.65;
  }
  /* A request needs a decision, so it gets the accent an ordinary row doesn't. */
  .agent.pending {
    border-left: 3px solid var(--primary-color);
  }
  .agent-main {
    flex: 1 1 260px;
    min-width: 0;
  }
  .agent-main h4 {
    margin: 0 0 2px;
    font-size: 0.95rem;
  }
  .agent-sub {
    margin: 0;
    font-size: 0.82rem;
    color: var(--secondary-text-color);
  }
  .node {
    margin-left: 8px;
    font-family: var(--code-font-family, monospace);
    font-size: 0.76rem;
    opacity: 0.75;
  }
  .agent-code {
    font-family: var(--code-font-family, monospace);
    font-size: 1.15rem;
    letter-spacing: 0.18em;
    padding: 6px 12px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--primary-color) 12%, transparent);
    color: var(--primary-color);
    white-space: nowrap;
  }
  .agent-actions {
    display: flex;
    gap: 8px;
    margin-left: auto;
  }
</style>
