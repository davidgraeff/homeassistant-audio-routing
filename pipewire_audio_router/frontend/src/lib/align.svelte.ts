// Latency-alignment session state, shared by every source card on the Sources
// page (SourcesTab → AlignPanel).
//
// The daemon allows exactly one alignment session at a time (it mutes the
// group's other members and loops a click through the group's anchor), so the
// session can't live in a per-card component: one module owns it and each card
// renders the slice for its own sync group.
//
// A sync group is identified by its *source set* — the outputs fed by exactly
// those sources play off one clock (see bridge-daemon/src/calibrate.rs). That's
// why alignment hangs off a source card: "these speakers are playing me right
// now, align them against each other".

import { api } from './api';
import { run } from './toast';
import type { AlignGroup, AlignMember, AlignState } from './types';

/** Highest offset the slider offers, per member kind (ms). */
export function sliderMax(m: AlignMember): number {
  return m.kind === 'sendspin' ? 2000 : 5000;
}

const sameSet = (a: string[], b: string[]) =>
  a.length === b.length && [...a].sort().join('|') === [...b].sort().join('|');

function createAlign() {
  let groups = $state<AlignGroup[]>([]);
  let session = $state<AlignState | null>(null);
  let loading = $state(true);
  let busy = $state(false);
  // Current per-member offset in ms (sendspin static delay / AirPlay 2 render delay).
  let offsets = $state<Record<string, number>>({});
  // Audible-member playback level (0–100), mirrored to the daemon.
  let level = $state(50);
  // Whether sendspin firmware applies a delay change live (Settings). When
  // false, a change reconnects that one speaker, so we don't stream during drag.
  let sendspinDelayLive = $state(false);

  async function seedOffsets() {
    const next: Record<string, number> = {};
    try {
      const [delays, outs] = await Promise.all([
        api.sendspinDelays().catch(() => ({}) as Record<string, number>),
        api.outputs().catch(() => []),
      ]);
      for (const m of session?.members ?? []) {
        if (m.kind === 'sendspin') next[m.node_name] = delays[m.node_name] ?? 0;
        else next[m.node_name] = outs.find((o) => o.node_name === m.node_name)?.latency_ms ?? 0;
      }
    } catch {
      /* leave defaults */
    }
    offsets = next;
  }

  /** Full load: session state, the alignable groups, and the delay-live setting. */
  async function refresh() {
    try {
      const [st, gs, settings] = await Promise.all([
        api.alignStatus(),
        api.alignGroups(),
        api.settings().catch(() => null),
      ]);
      session = st;
      groups = gs;
      if (settings) sendspinDelayLive = settings.sendspin_delay_live;
      if (st.active) {
        level = st.volume;
        await seedOffsets();
      }
    } catch {
      session = null;
      groups = [];
    }
    loading = false;
  }

  /** Cheap poll: which groups are alignable changes with routing and presence. */
  async function refreshGroups() {
    try {
      groups = await api.alignGroups();
    } catch {
      /* keep last-known */
    }
  }

  // Live drag: sendspin delay is applied in-band and takes effect immediately,
  // so push while dragging (throttled). AirPlay 2 render delay is committed on
  // release (onchange) — dragging just updates the readout.
  let throttleTimer: ReturnType<typeof setTimeout> | null = null;
  let pending: { m: AlignMember; ms: number } | null = null;

  const ctl = {
    get groups() {
      return groups;
    },
    get session() {
      return session;
    },
    get loading() {
      return loading;
    },
    get busy() {
      return busy;
    },
    get offsets() {
      return offsets;
    },
    get level() {
      return level;
    },
    get sendspinDelayLive() {
      return sendspinDelayLive;
    },

    refresh,
    refreshGroups,

    /** Mount hook: load, keep the group list fresh, and never leave a session
     *  running behind a page the user navigated away from. */
    attach(): () => void {
      void refresh();
      const timer = setInterval(refreshGroups, 5000);
      return () => {
        clearInterval(timer);
        if (session?.active) void api.alignStop().catch(() => {});
        session = null;
      };
    },

    /** The alignable group `sourceNodeName` currently feeds, if any. */
    groupForSource(sourceNodeName: string): AlignGroup | undefined {
      return groups.find((g) => g.sources.includes(sourceNodeName));
    },
    /** Whether the running session is this group's. */
    isActive(g: AlignGroup): boolean {
      return !!session?.active && sameSet(session.sources, g.sources);
    },
    /** Whether *another* group's session is running (blocks starting this one). */
    isBlocked(g: AlignGroup): boolean {
      return !!session?.active && !sameSet(session.sources, g.sources);
    },

    async start(g: AlignGroup) {
      busy = true;
      try {
        session = await api.alignStart(g.sources);
        level = session.volume;
        await seedOffsets();
      } catch (e) {
        await run(() => Promise.reject(e));
      }
      busy = false;
    },

    async stop() {
      busy = true;
      if (await run(() => api.alignStop(), 'Alignment finished — volumes restored')) {
        session = { active: false, sources: [], reference: null, target: null, members: [], volume: level };
      }
      busy = false;
    },

    /** Drag feedback for the volume slider — readout only, no request. */
    previewLevel(v: number) {
      level = v;
    },

    /** Commit the playback level (on slider release). */
    async setLevel(v: number) {
      level = v;
      try {
        session = await api.alignVolume(v);
      } catch (e) {
        await run(() => Promise.reject(e));
      }
    },

    /** Pick a member as the reference; keep a distinct target audible alongside it. */
    async setReference(m: AlignMember) {
      if (!session) return;
      let target = session.target;
      if (target === null || target === m.node_name) {
        target = session.members.find((x) => x.node_name !== m.node_name)?.node_name ?? m.node_name;
      }
      await select(m.node_name, target);
    },

    /** Make `m` the speaker being tuned (audible with the reference). */
    async tune(m: AlignMember) {
      if (!session || session.reference === null || session.reference === m.node_name) return;
      await select(session.reference, m.node_name);
    },

    /** Commit a member's offset (the persisted per-device delay knob). */
    async applyOffset(m: AlignMember, ms: number) {
      const clamped = Math.max(0, Math.min(sliderMax(m), Math.round(ms)));
      offsets = { ...offsets, [m.node_name]: clamped };
      try {
        if (m.kind === 'sendspin') await api.setSendspinDelay(m.node_name, clamped);
        else await api.setOutputLatency(m.node_name, clamped);
      } catch (e) {
        await run(() => Promise.reject(e));
      }
    },

    /** Drag feedback: update the readout, and stream the value only when the
     *  device applies it live (else a change would reconnect it mid-drag). */
    liveOffset(m: AlignMember, ms: number) {
      offsets = { ...offsets, [m.node_name]: ms }; // immediate readout
      if (m.kind !== 'sendspin' || !sendspinDelayLive) return;
      pending = { m, ms };
      if (throttleTimer) return;
      throttleTimer = setTimeout(() => {
        throttleTimer = null;
        if (pending) {
          const p = pending;
          pending = null;
          void ctl.applyOffset(p.m, p.ms);
        }
      }, 100);
    },
  };

  async function select(reference: string, target: string) {
    try {
      session = await api.alignSelect(reference, target);
    } catch (e) {
      await run(() => Promise.reject(e));
    }
  }

  return ctl;
}

export const align = createAlign();
