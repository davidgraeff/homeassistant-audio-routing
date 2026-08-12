// Latency-alignment session state, shared by every source card on the Sources
// page (SourcesTab → AlignPanel).
//
// The daemon allows exactly one alignment session at a time (it mutes the
// group's other members and loops a click through the group's anchor), so the
// session can't live in a per-card component: one module owns it and each card
// renders the slice for its own sync group.
//
// A sync group is identified by its *source set* — the outputs fed by exactly
// those sources play off one clock (see bridge-daemon/src/align/calibrate.rs). That's
// why alignment hangs off a source card: "these speakers are playing me right
// now, align them against each other".

import { api } from './api';
import { run } from './toast';
import { mic } from './mic.svelte';
import type {
  AlignGroup,
  AlignMember,
  AlignMemberKind,
  AlignSessionMode,
  AlignState,
  MeasureMode,
  OutputInfo,
  SignalCheck,
} from './types';

/** The level a measured run sets speakers to while their levels are being checked
 *  (plan §12.2): 20 %, not the by-ear panel's old 50 %.
 *
 *  Real in-room readings came out roughly 40 dB above what the estimator needs, so
 *  50 % is needlessly loud for something done standing in a living room. The daemon's
 *  `DEFAULT_ALIGN_LEVEL` is the same 20, so this agrees with it rather than fighting
 *  it — it is stated here because the *slider* needs a starting position too. */
export const DEFAULT_MEASURE_LEVEL = 20;

const KIND_LABELS: Record<AlignMemberKind, string> = {
  sendspin: 'Sendspin',
  airplay2: 'AirPlay 2',
  pwsink: 'PipeWire host',
};

/** One member kind, in the user's words. Centralised because "not sendspin ⇒
 *  AirPlay 2" stopped being true when pw-sink became an alignable kind. */
export function memberKindLabel(kind: AlignMemberKind): string {
  return KIND_LABELS[kind] ?? kind;
}

/** Who owns a member's playback level during a session — i.e. whether the wizard's
 *  level slider can do anything at all for it.
 *
 *  Three different answers, checked against `calibrate::apply_audibility` rather than
 *  assumed, because a slider that silently does nothing is the worst of the three:
 *
 *  * `session` (sendspin) — the calibration level is pushed per device and restored on
 *    teardown. The slider is real.
 *  * `receiver` (AirPlay 2) — the session mutes and unmutes, but deliberately does
 *    **not** impose a volume: an AP2 receiver's level is device-authoritative, and the
 *    session snapshots only its mute, so a level written here could not be put back.
 *    So the level is set on the receiver, and this page's job is to *judge* it.
 *  * `none` (pw-sink) — no level and no in-band mute at all. It cannot be tuned or
 *    silenced from here, so it keeps playing through every other member's turn and
 *    constrains what their levels have to be (plan §7, §12.3.2). */
export type LevelControl = 'session' | 'receiver' | 'none';

export function levelControl(kind: AlignMemberKind): LevelControl {
  if (kind === 'sendspin') return 'session';
  return kind === 'airplay2' ? 'receiver' : 'none';
}

/** Which way raising this kind's knob moves the speaker (plan §2.4.1).
 *
 *  Mirrors the daemon's kind → polarity mapping (`align::measure::KnobPolarity`) for
 *  the places that have only a member kind to go on — the by-ear panel and the run's
 *  per-member progress. Where the daemon *sends* a polarity or an `effect` sentence
 *  (`ProposedDelay`), that is authoritative and this must not be used instead.
 *
 *  Not a cosmetic label: a sendspin device subtracts its `static_delay_ms` from the
 *  playback instant, so a UI that calls that knob a delay tells the user the exact
 *  opposite of what the slider will do. */
export function knobNoun(kind: AlignMemberKind): 'advance' | 'delay' {
  return kind === 'sendspin' ? 'advance' : 'delay';
}

/** Lowest offset the delay slider may offer (ms).
 *
 *  A PipeWire receiver's playout delay is its jitter buffer, and it cannot go below
 *  three packet times (`sync_settings::PWSINK_JITTER_MIN_MS` = 3 × 5 ms), so a
 *  slider that starts at zero would promise a placement the receiver refuses. */
export function sliderMin(m: { kind: AlignMemberKind }): number {
  return m.kind === 'pwsink' ? 15 : 0;
}

/** Highest offset the slider offers, per member kind (ms). */
export function sliderMax(m: { kind: AlignMemberKind }): number {
  if (m.kind === 'sendspin') return 2000;
  return m.kind === 'pwsink' ? 2000 : 5000;
}

/** The session promise (`AlignMode`) a measurement mode makes. Two enums, one
 *  meaning: `sweet_spot` and `multi_position` are the same promise under the two
 *  names the daemon uses (`MeasureMode` vs `AlignMode`), so the mapping is written
 *  once here instead of guessed at each call site. */
export function sessionModeFor(mode: MeasureMode): AlignSessionMode {
  return mode === 'near_field' ? 'near_field' : 'multi_position';
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

  // ---- The wizard's speaker selection (plan §12.1, §12.2, §12.3.1) ----------
  //
  // This lives beside the session rather than in the page component because the
  // page is unmounted every time the user steps back to the mode picker, and losing
  // the scope (and the levels checked so far) to a navigation would be absurd.

  /** The adopted outputs the picker offers. */
  let outputs = $state<OutputInfo[]>([]);
  let outputsLoaded = $state(false);
  /** The run's **entire scope** — every speaker the one hold will cover, not the
   *  first position's subset (plan §12.3.1). */
  let selection = $state<string[]>([]);
  /** Per-speaker playback level for level-setting, remembered while the wizard is
   *  open so re-soloing a speaker returns to the level it was left at. The daemon
   *  keeps one session level, applied to whoever is audible, so this side is what
   *  makes it read as per-speaker. */
  let levels = $state<Record<string, number>>({});
  /** The speaker this browser last asked to hear. Not the whole truth — see the
   *  `soloed` getter, which reconciles it with what the daemon says is audible. */
  let soloIntent = $state<string | null>(null);
  /** The last level verdict seen for each speaker while it was soloed — the
   *  checklist of "which of my scope has been confirmed audible". */
  let verdicts = $state<Record<string, SignalCheck['verdict']>>({});

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

  /** The speaker actually playing the tone: this browser's own last instruction while
   *  the daemon still agrees it is audible, else the daemon's single audible member.
   *  One definition, used by the getter and by the level controls, so they can never
   *  disagree about which speaker a level belongs to. */
  function currentSolo(): string | null {
    const audible = session?.audible ?? [];
    if (soloIntent && (audible.length === 0 || audible.includes(soloIntent))) return soloIntent;
    return audible.length === 1 ? audible[0] : null;
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

  /** Session state only — no groups, no offsets, no settings.
   *
   *  Wanted because parts of `AlignState` change *without* the UI doing anything:
   *  `interference` records a doorbell or a voice-assistant turn that outranked the
   *  session's exclusive hold, and that has to be visible while it is still relevant
   *  rather than the next time something else happens to refresh. */
  async function refreshStatus() {
    try {
      // Deliberately does not adopt `volume`: this runs on a timer, and a poll
      // landing mid-drag would snap the slider back under the user's finger. The
      // level the *daemon* used per member is reported by the measurement status.
      session = await api.alignStatus();
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

    /** Is a session holding speakers right now (however it was started)? */
    get sessionActive() {
      return !!session?.active;
    },
    /** The adopted outputs the wizard's picker offers. */
    get outputs() {
      return outputs;
    },
    get outputsLoaded() {
      return outputsLoaded;
    },
    /** The run's whole scope, as picked. */
    get selection() {
      return selection;
    },
    get levels() {
      return levels;
    },
    /** The speaker whose tone is playing, or null when the group is silent.
     *
     *  Our own last instruction while the daemon still agrees it is audible (so the
     *  highlight and the slider do not flicker on a status poll that was already in
     *  flight), and otherwise whatever the daemon reports as its single audible
     *  member — a measurement run moves the solo itself, and after one has been
     *  abandoned this page must show what is *actually* playing rather than what this
     *  browser last asked for. */
    get soloed(): string | null {
      return currentSolo();
    },
    get verdicts() {
      return verdicts;
    },
    /** The level a speaker plays at. This browser's choice if it made one; otherwise
     *  the *session's* level for whichever speaker is actually audible — that is the
     *  level really being applied, and showing the default instead would misreport it
     *  after a reload or a run that set its own. */
    levelOf(nodeName: string): number {
      const own = levels[nodeName];
      if (own !== undefined) return own;
      return currentSolo() === nodeName ? (session?.volume ?? DEFAULT_MEASURE_LEVEL) : DEFAULT_MEASURE_LEVEL;
    },
    /** The kind of a selected output, for the knobs it does and does not have. */
    kindOf(nodeName: string): AlignMemberKind | null {
      const m = session?.members.find((x) => x.node_name === nodeName);
      if (m) return m.kind;
      const o = outputs.find((x) => x.node_name === nodeName);
      return o ? o.kind : null;
    },

    /** Things that outranked the session's exclusive hold (plan §12.3), newest
     *  last. Never empty-checked away: an interfering doorbell is the explanation
     *  for a reading the gate would otherwise blame on the user's hand. */
    get interference() {
      return session?.interference ?? [];
    },
    /** Routing the session is displacing while it holds these speakers. */
    get displaced() {
      return session?.displaced ?? [];
    },

    refresh,
    refreshGroups,
    refreshStatus,

    /** Mount hook for the **by-ear** panel: load, keep the group list fresh, and never
     *  leave a by-ear session running behind a page the user navigated away from.
     *
     *  The teardown is deliberately narrow: it stops a `manual` session only. There is one
     *  session process-wide but it is no longer always this page's — the microphone wizard
     *  lives on the Outputs page now (plan §12.1), and its session's mode is
     *  `multi_position` / `near_field`. Stopping *any* session here would mean a user who
     *  glanced at the Sources page mid-run came back to a measurement that had been torn
     *  down, its provisional delays discarded, by a page that had nothing to do with it.
     *  A by-ear session, by contrast, can only have been started from here. */
    attach(): () => void {
      void refresh();
      const timer = setInterval(refreshGroups, 5000);
      return () => {
        clearInterval(timer);
        if (session?.active && session.mode === 'manual') void api.alignStop().catch(() => {});
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

    /** Load the adopted outputs the picker offers. Cheap and idempotent; the picker
     *  calls it on mount and after adopting something elsewhere. */
    async loadOutputs() {
      try {
        outputs = await api.outputs();
      } catch {
        /* keep last-known; the picker says when it has nothing */
      }
      outputsLoaded = true;
    },

    /** Add or drop one speaker from the run's scope. Refused once a hold exists:
     *  the scope *is* the hold, and changing it means paying the reconnect wave
     *  twice more (plan §12.3.1), so that is a deliberate stop-and-restart rather
     *  than a click. */
    toggleSelected(nodeName: string) {
      if (session?.active) return;
      selection = selection.includes(nodeName) ? selection.filter((n) => n !== nodeName) : [...selection, nodeName];
    },
    /** Seed the scope, e.g. from the speakers a source is already playing to. */
    setSelection(nodeNames: string[]) {
      if (session?.active) return;
      selection = [...new Set(nodeNames)];
    },
    clearSelection() {
      if (session?.active) return;
      selection = [];
    },

    /** Form the temporary exclusive group over the **whole** selection and go quiet.
     *
     *  Silence is deliberate: forming a group leaves the daemon's first two members
     *  audible, and the level phase is one speaker at a time (plan §12.2). Starting
     *  with two speakers playing would both mis-set levels and contradict what the
     *  page says it is doing. */
    async startSelection(mode: MeasureMode): Promise<boolean> {
      if (selection.length < 2) return false;
      busy = true;
      try {
        session = await api.alignStartOutputs(selection, sessionModeFor(mode));
        level = session.volume;
        soloIntent = null;
        await seedOffsets();
        session = await api.alignAudible([], level);
        busy = false;
        return true;
      } catch (e) {
        busy = false;
        await run(() => Promise.reject(e));
        return false;
      }
    },

    /** Play the tone on exactly one speaker (plan §12.2's solo), at that speaker's
     *  own level. Clicking the same speaker again, or `stopTone()`, silences it. */
    async solo(nodeName: string) {
      const next = levels[nodeName] ?? DEFAULT_MEASURE_LEVEL;
      levels = { ...levels, [nodeName]: next };
      soloIntent = nodeName;
      level = next;
      // The trailing signal window still holds the previous speaker's sound.
      mic.disturbSignal();
      try {
        session = await api.alignAudible([nodeName], next);
      } catch (e) {
        soloIntent = null;
        await run(() => Promise.reject(e));
      }
    },

    /** Play the tone on exactly this **set** of members, muting the rest — plan §12.2's
     *  solo generalised, which is also how a chain scopes a listening position (§12.3.1).
     *
     *  Wanted separately from `solo()` because a position is a set, and because the
     *  question it answers is different: not "is this speaker's level right" but "can I
     *  actually hear all of these from where I am standing, and are the overlaps among
     *  them?". Mutes are live, so this costs nothing and no speaker reconnects — which is
     *  the whole reason the run holds one group and scopes positions this way.
     *
     *  Note the daemon does **not** drive audibility between positions itself (W12), so
     *  whatever was last made audible stays that way until the run solos for a
     *  measurement. `soloIntent` is cleared: what is playing is a set, not one speaker's
     *  level being judged, and leaving the intent set would make the level slider claim
     *  one of them. */
    async hear(nodeNames: string[]) {
      if (!session?.active) return;
      soloIntent = null;
      mic.disturbSignal();
      try {
        session = await api.alignAudible(nodeNames, level);
      } catch (e) {
        await run(() => Promise.reject(e));
      }
    },

    /** Silence every member, leaving the session and the hold in place. */
    async stopTone() {
      if (!session?.active) return;
      soloIntent = null;
      mic.disturbSignal();
      try {
        session = await api.alignAudible([], level);
      } catch {
        /* nothing to say: the session may already be gone, which is also silence */
      }
    },

    /** Drag feedback for the soloed speaker's level — readout only. */
    previewSoloLevel(v: number) {
      const node = currentSolo();
      if (!node) return;
      levels = { ...levels, [node]: v };
      level = v;
    },

    /** Commit the soloed speaker's level (on release). */
    async setSoloLevel(v: number) {
      const node = currentSolo();
      if (!node) return;
      levels = { ...levels, [node]: v };
      level = v;
      mic.disturbSignal();
      try {
        session = await api.alignAudible([node], v);
      } catch (e) {
        await run(() => Promise.reject(e));
      }
    },

    /** Remember the verdict just seen for the speaker being tuned, so the page can
     *  show which of the scope has been confirmed rather than only the current one. */
    recordVerdict(nodeName: string, verdict: SignalCheck['verdict']) {
      if (verdicts[nodeName] === verdict) return;
      verdicts = { ...verdicts, [nodeName]: verdict };
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
      soloIntent = null;
      // The verdicts described *this* hold's levels; carrying them into the next one
      // would show a confirmation that nothing has re-checked. Levels are kept —
      // they are the user's choice, not a measurement.
      verdicts = {};
      if (await run(() => api.alignStop(), 'Alignment finished — volumes restored')) {
        session = {
          active: false,
          sources: [],
          reference: null,
          target: null,
          members: [],
          volume: level,
          // Optimistic inactive state: the daemon's own `inactive()` reports an empty
          // map, and no member has a session-applied level once the session is gone.
          levels: {},
          mode: 'manual',
          outputs: [],
          audible: [],
          interference: [],
          displaced: [],
        };
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
      const clamped = Math.max(sliderMin(m), Math.min(sliderMax(m), Math.round(ms)));
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
