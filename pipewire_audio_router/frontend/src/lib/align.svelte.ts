// The latency-alignment **session**: the speakers held exclusively for an alignment,
// the click looping through them, and who is audible at what level.
//
// The daemon allows exactly one session at a time, process-wide, so it cannot live in
// a component: this module owns it and the wizard, on the Alignment page, renders it.
//
// **Its identity is a set of speakers, not a source set** (plan §12.1, §12.3.1). It
// used to be the other way round — a group was resolved from the sources feeding it,
// which is why the panel lived on source cards — and every trace of that framing is
// gone from here on purpose: with one session process-wide, a second way to name it is
// how two pages come to believe they each own it. All three modes (§1) go through
// `startSelection`, including by-ear, which is what stops `manual` being a special
// case with its own entry point and its own lifecycle rules.

import { api } from './api';
import { run } from './toast';
import { mic } from './mic.svelte';
import type {
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

/** What the wizard is doing: one of the two measured modes, or the by-ear path.
 *
 *  Deliberately *not* `MeasureMode` widened. `MeasureMode` is the measurement state
 *  machine's own enum and `manual` is not one of its values — no run is started, no
 *  microphone is needed, nothing is proposed. But `manual` **is** one of plan §1's
 *  three modes and one of the daemon's `AlignMode`s, so it is a mode of the *wizard*:
 *  same speaker selection, same temporary exclusive hold (§12.3.1), different body. */
export type WizardMode = MeasureMode | 'manual';

/** Whether this wizard mode drives the microphone measurement state machine. Where it
 *  is false, `measure.*` must not be called at all: `POST /api/align/measure/start`
 *  has no `manual` mode and would refuse. */
export function isMeasured(mode: WizardMode): mode is MeasureMode {
  return mode !== 'manual';
}

/** The session promise (`AlignMode`) a wizard mode makes. Two enums, one meaning:
 *  `sweet_spot` and `multi_position` are the same promise under the two names the
 *  daemon uses (`MeasureMode` vs `AlignMode`), so the mapping is written once here
 *  instead of guessed at each call site. */
export function sessionModeFor(mode: WizardMode): AlignSessionMode {
  if (mode === 'manual') return 'manual';
  return mode === 'near_field' ? 'near_field' : 'multi_position';
}

function createAlign() {
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

  /** Full load: session state, the delay-live setting, and the per-member knob values a
   *  by-ear session's sliders start from. */
  async function refresh() {
    try {
      const [st, settings] = await Promise.all([api.alignStatus(), api.settings().catch(() => null)]);
      session = st;
      if (settings) sendspinDelayLive = settings.sendspin_delay_live;
      if (st.active) {
        level = st.volume;
        await seedOffsets();
      }
    } catch {
      session = null;
    }
    loading = false;
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
    refreshStatus,

    /** Mount hook for the wizard: load the session, the knob values its by-ear sliders
     *  start from, and the delay-live setting, then keep the session state fresh.
     *
     *  **It deliberately stops nothing on teardown.** The old hook stopped a session whose
     *  mode was `manual`, on the theory that a by-ear session could only have been started
     *  by the page being unmounted — true while by-ear lived on source cards, and false
     *  now that `manual` is one of the wizard's three modes (plan §1). Keeping that rule
     *  would mean a by-ear hold, formed in the wizard like every other, being torn
     *  down by *another* page unmounting; and applying it here would tear one down the
     *  moment the user switched tabs mid-tuning. So there is exactly one way a session
     *  ends: someone asks for it to end (`stop()`, the wizard's "Stop and restore"), or the
     *  daemon's own idle timeout does it. */
    attachSession(): () => void {
      void refresh();
      const timer = setInterval(refreshStatus, 3000);
      return () => clearInterval(timer);
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

    /** Form the temporary exclusive group over the **whole** selection (plan §12.3.1).
     *
     *  For a **measured** mode it then goes quiet, deliberately: forming a group leaves
     *  the daemon's first two members audible, and the level phase is one speaker at a
     *  time (plan §12.2), so starting with two of them playing would both mis-set levels
     *  and contradict what the page says it is doing.
     *
     *  For **by-ear** the opposite is true — the whole method is comparing two speakers
     *  against each other — so the pair the daemon made audible when it formed the hold
     *  (its first two members, as reference and target) is left playing. Silencing it and
     *  making the user find the play button would be a step that exists only because the
     *  other two modes need it. */
    async startSelection(mode: WizardMode): Promise<boolean> {
      if (selection.length < 2) return false;
      busy = true;
      try {
        session = await api.alignStartOutputs(selection, sessionModeFor(mode));
        level = session.volume;
        soloIntent = null;
        await seedOffsets();
        if (isMeasured(mode)) session = await api.alignAudible([], level);
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
