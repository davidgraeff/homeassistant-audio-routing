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

import { ALIGN_WS_PATH, api, wsUrl } from './api';
import { run } from './toast';
import { mic } from './mic.svelte';
import type {
  AlignMember,
  AlignMemberKind,
  AlignSessionMode,
  AlignState,
  LevelChannel,
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

/** Whether the wizard's level slider can do anything for a member — **derived from the
 *  daemon's resolved `LevelChannel`, never from the member's kind.**
 *
 *  This used to be a kind → answer table, and both of its interesting rows went stale
 *  without anything failing:
 *
 *  * **AirPlay 2 was "the receiver's own, we never write it"**, which stopped being true
 *    when the session started driving AP2 volume for its duration (W18, plan §7's
 *    decision table). It writes a *transient* level and gives the receiver's own back at
 *    teardown, so the slider is real — it just borrows the level rather than owning it.
 *  * **pw-sink was "no level and no mute at all"**, which was never quite true and is now
 *    plainly wrong (W20): a pw-sink host's level is its receiver agent's, and the daemon
 *    asks the agent per position rather than assuming. And the *mute* has a universal
 *    fallback — `relay_delay` can silence anything, cooperation or not (W17) — so nothing
 *    is ever un-mutable and no UI should say so.
 *
 *  The lesson the daemon already documents (`AlignState::level_channels`): this is a
 *  per-output capability, so two members of one kind differ and one member's answer
 *  changes when its agent drops mid-walk. Hence four answers, one of which is "not
 *  resolved yet" rather than a guess:
 *
 *  * `live` — set here, applied as you drag, restored at teardown (sendspin).
 *  * `borrowed` — set here for the run and put back afterwards (an AP2 receiver, or a
 *    PipeWire host through its agent). The slider works; the level is not the user's
 *    stored one.
 *  * `none` — nothing this daemon can reach. Not tunable, still silenceable, and it sets
 *    the clip ceiling every other member has to fit under (plan §7).
 *  * `unresolved` — no session yet, or a member that has not been through an audibility
 *    pass. Say nothing rather than guessing; the answer arrives with the hold. */
export type LevelControl = 'live' | 'borrowed' | 'none' | 'unresolved';

export function levelControl(channel: LevelChannel | undefined): LevelControl {
  if (channel === undefined) return 'unresolved';
  if (channel === 'sendspin_live') return 'live';
  return channel === 'none' ? 'none' : 'borrowed';
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

// ---- The session's idle timeout, as something a user can see coming ----------
//
// The hold is exclusive, so when the daemon's idle timeout fires, speakers that were
// held go back to normal — and a wizard still on screen is describing a session that no
// longer exists. It bit a real multi-position run, because the step where a session
// runs out is the *review page* and reading is silent.

/** How this browser understands a session ending. */
export type SessionEndCause =
  /** We asked for it (`stop()`), so the toast has already said so. */
  | 'stopped-here'
  /** It ran out of idle time. Inferred from the last countdown we were told, so it is a
   *  best reading rather than a claim the daemon made. */
  | 'timed-out'
  /** Something else ended it: another tab's Stop, or a `start` that superseded it. */
  | 'elsewhere';

export interface SessionEnd {
  cause: SessionEndCause;
  /** What to show. Written here rather than in a component because two pages show it
   *  (the wizard and the Outputs page's held notice) and one wording is the point. */
  why: string;
}

/** The remaining idle time, in the user's words — deliberately vague.
 *
 *  The daemon's watchdog only looks every `timeout_slack_s` (30 s), so the real close can
 *  be that much later than the number it sent. Rendering "13:42" would promise a
 *  precision that does not exist and invite someone to watch it, so this rounds to
 *  whatever the slack can support and always says "about". */
export function holdCloseLabel(seconds: number, slackSeconds: number): string {
  if (seconds <= 0) return 'any moment now';
  if (seconds <= slackSeconds) return 'in less than a minute';
  const minutes = Math.round(seconds / 60);
  if (minutes <= 1) return 'in about a minute';
  return `in about ${minutes} minutes`;
}

/** Below this many seconds left, the indicator stops being a footnote. Two minutes is
 *  four watchdog polls — enough that a warning is still actionable, close enough that it
 *  is worth interrupting someone reading a proposal. */
export const HOLD_CLOSE_WARN_S = 120;

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

  // ---- The session is pushed, with polling as the floor -----------------------
  //
  // The same two-channel shape `measure.svelte.ts` uses, and for a sharper reason: a
  // session can end *without the UI doing anything at all* — the daemon's idle timeout
  // gives the speakers back, a second tab stops it, a new `start` supersedes it — and
  // until this browser hears about that it is showing a wizard for a session that no
  // longer exists while the speakers it names have already gone back to normal.
  //
  //   * polling starts immediately on attach and keeps going;
  //   * the socket only *earns* the right to stop it by delivering an actual state, so an
  //     upgrade that succeeds and then says nothing changes nothing;
  //   * a close or an error puts polling back and re-tries the socket later.
  let socket: WebSocket | null = null;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let statusTimer: ReturnType<typeof setTimeout> | null = null;
  let watching = 0;
  /** True once the socket has actually delivered a state, i.e. polling is off. */
  let pushing = $state(false);

  /** Fallback poll interval. `AlignState` also carries things nothing pushes —
   *  `interference` is appended by the announce arbiter through the hold, which has no way
   *  back to the session's notifier — so this stays modest rather than becoming a
   *  once-a-minute formality. */
  const STATUS_POLL_MS = 3000;
  /** Quiet upgrade attempt after the socket failed or dropped. Polling continues
   *  throughout, so nobody is waiting on it. */
  const WS_RETRY_MS = 15000;

  // ---- The idle countdown, ticked locally from a pushed deadline ---------------
  //
  // `closes_in_s` is relative to the frame it arrived in, so the moment it arrives it is
  // turned into a deadline on *this* clock and counted down here. A per-second push would
  // be absurd, and an absolute daemon timestamp would be wrong by whatever the two clocks
  // disagree by.
  let closeDeadline = $state<number | null>(null);
  let ticked = $state(Date.now());
  let tick: ReturnType<typeof setInterval> | null = null;
  /** Set while our own `stop()` is in flight, so the state it clears is not reported back
   *  to the user as something that happened *to* them. */
  let stopping = false;
  let ended = $state<SessionEnd | null>(null);

  function startTicking() {
    if (tick) return;
    tick = setInterval(() => (ticked = Date.now()), 1000);
  }

  function stopTicking() {
    if (!tick) return;
    clearInterval(tick);
    tick = null;
  }

  /** Seconds left before the session is torn down, counted down locally and re-synced by
   *  every frame. `null` when nothing is running, or when a daemon too old to send the
   *  field is answering — in which case the UI says nothing rather than inventing one. */
  function closesIn(): number | null {
    if (closeDeadline === null) return null;
    return Math.max(0, Math.round((closeDeadline - ticked) / 1000));
  }

  /** Adopt one state frame, from either channel, and notice the transitions that matter.
   *
   *  Every assignment to `session` goes through here. Two of them are not bookkeeping:
   *  the local deadline is re-derived from `closes_in_s` on every frame (so a refresh
   *  anywhere — this tab, another tab, the run itself soloing a speaker — moves the
   *  countdown), and an `active` that has just gone false runs the reset below. */
  function adopt(st: AlignState) {
    const was = session?.active ?? false;
    // How much idle time was left *this instant*, from the deadline rather than from the
    // once-a-second tick. Taken before the deadline is dropped below, because it is what
    // tells a session that has just vanished apart from one somebody else stopped.
    const remainingNow = closeDeadline === null ? null : Math.max(0, Math.round((closeDeadline - Date.now()) / 1000));
    session = st;
    if (st.active) {
      ended = null;
      closeDeadline = st.closes_in_s === null || st.closes_in_s === undefined ? null : Date.now() + st.closes_in_s * 1000;
      if (closeDeadline !== null) startTicking();
      return;
    }
    closeDeadline = null;
    stopTicking();
    if (was) sessionClosed(st, remainingNow);
  }

  /** The session has gone. Put everything derived from it back to a state the user can
   *  start again from, and say why — a panel that silently empties leaves someone
   *  wondering whether they broke it.
   *
   *  What is **not** cleared is the two things that are the user's own choices rather than
   *  the session's: the speaker selection (so "start again" is one click, which is the
   *  whole point of resetting properly) and the per-speaker levels (they were chosen, not
   *  measured — the same reasoning `stop()` has always used for them). */
  function sessionClosed(st: AlignState, remainingNow: number | null) {
    soloIntent = null;
    // The verdicts described *that* hold's levels; carrying them into the next one would
    // show a confirmation nothing has re-checked. The level slider is left where the user
    // put it — it is their choice, and it seeds the next session.
    verdicts = {};
    if (stopping) {
      // We asked for it, and `run()` has already said "volumes restored".
      ended = null;
      return;
    }
    // The countdown had run out (or was inside one watchdog poll of it): the idle timeout
    // is the explanation. Anything else was ended somewhere else. It is an inference, not
    // a claim the daemon made, which is why the two sentences differ in what they promise
    // — one explains a rule, the other only reports that it happened.
    const slack = st.timeout_slack_s ?? 30;
    const timedOut = remainingNow !== null && remainingNow <= slack;
    const minutes = Math.round((st.idle_timeout_s ?? 900) / 60);
    ended = timedOut
      ? {
          cause: 'timed-out',
          why:
            `The alignment gave the speakers back on its own: nothing had changed for ${minutes} minutes, so it released them ` +
            `and put their levels, mutes and routing back. Reading a page does not count as a change — soloing a speaker, ` +
            `moving a level or measuring one does, and so does “Keep it open”. Everything you had picked is still selected, ` +
            `so you can start again.`,
        }
      : {
          cause: 'elsewhere',
          why:
            'The alignment session ended somewhere else — another tab stopped it, or a new one was started over it. The ' +
            'speakers have their levels, mutes and routing back. Your selection is still here, so you can start again.',
        };
  }

  async function pollStatus() {
    try {
      adopt(await api.alignStatus());
    } catch {
      /* keep the last-known state: a failed poll is not a state change */
    }
    scheduleStatusPoll();
  }

  function scheduleStatusPoll() {
    if (statusTimer) clearTimeout(statusTimer);
    statusTimer = watching > 0 && !pushing ? setTimeout(() => void pollStatus(), STATUS_POLL_MS) : null;
  }

  function scheduleSocketRetry() {
    if (retry || watching === 0) return;
    retry = setTimeout(() => {
      retry = null;
      openSocket();
    }, WS_RETRY_MS);
  }

  function openSocket() {
    if (socket || watching === 0) return;
    let sock: WebSocket;
    try {
      sock = new WebSocket(wsUrl(ALIGN_WS_PATH));
    } catch {
      scheduleSocketRetry();
      return;
    }
    socket = sock;
    sock.onmessage = (ev) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(String(ev.data));
      } catch {
        return; // not a state frame; polling is still running if we never adopted one
      }
      // Shape-checked rather than trusted: adopting `{}` would report the session as
      // ended, which is the one thing this channel must never get wrong.
      if (!parsed || typeof parsed !== 'object' || typeof (parsed as AlignState).active !== 'boolean') return;
      adopt(parsed as AlignState);
      if (!pushing) {
        pushing = true;
        if (statusTimer) {
          clearTimeout(statusTimer);
          statusTimer = null;
        }
      }
    };
    sock.onclose = () => {
      if (socket !== sock) return;
      socket = null;
      pushing = false;
      // Straight back to polling — the session's *end* must not be missed because a
      // proxy dropped a socket — then a quiet attempt to get the socket back.
      void pollStatus();
      scheduleSocketRetry();
    };
    // `onerror` is always followed by `onclose`, which is where the recovery lives.
    sock.onerror = () => {};
  }

  function closeSocket() {
    if (retry) {
      clearTimeout(retry);
      retry = null;
    }
    pushing = false;
    if (!socket) return;
    const sock = socket;
    socket = null;
    sock.onclose = null;
    sock.onerror = null;
    sock.onmessage = null;
    sock.close(1000, 'no longer watching the alignment session');
  }

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
      adopt(st);
      if (settings) sendspinDelayLive = settings.sendspin_delay_live;
      if (st.active) {
        level = st.volume;
        await seedOffsets();
      }
    } catch {
      /* keep the last-known state: a failed load is not a session ending */
    }
    loading = false;
  }

  /** Session state only — no groups, no offsets, no settings.
   *
   *  Wanted because parts of `AlignState` change *without* the UI doing anything:
   *  `interference` records a doorbell or a voice-assistant turn that outranked the
   *  session's exclusive hold, and that has to be visible while it is still relevant
   *  rather than the next time something else happens to refresh; and the session can
   *  end on its own, which is the state this must never be stale about.
   *
   *  Note it deliberately does not adopt `volume` into the slider: a frame landing
   *  mid-drag would snap it back under the user's finger. The level the *daemon* used per
   *  member is reported by the measurement status. */
  async function refreshStatus() {
    try {
      adopt(await api.alignStatus());
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
    /** How this member's level is reached right now, as the daemon resolved it — or
     *  `undefined` before there is an answer (no session, or a member no audibility pass
     *  has covered yet).
     *
     *  The one place the UI learns whether a level slider is real. Deliberately not
     *  answerable from `kindOf`: see `levelControl`. */
    levelChannel(nodeName: string): LevelChannel | undefined {
      return session?.level_channels?.[nodeName];
    },
    /** The same answer as the wizard's three-way question. */
    levelControlOf(nodeName: string): LevelControl {
      return levelControl(session?.level_channels?.[nodeName]);
    },
    /** Held members with no level knob at all — the `none` channels above, as the daemon
     *  lists them. Its `level_note` beside them is deliberately *not* surfaced: it is
     *  written for an API caller (it quotes node labels and cites the plan), so the pages
     *  say the same thing in their own words. */
    get unlevellable(): string[] {
      return session?.unlevellable ?? [];
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

    /** Seconds before the session's idle timeout gives the speakers back, ticked locally
     *  from the deadline the daemon pushed. `null` when nothing is running.
     *
     *  **Reaching 0 is not the same as the session being over**: the daemon's watchdog
     *  only looks every `closeSlack` seconds, so the end is rendered when a frame says so
     *  (`ended`), never by this hitting zero. */
    get closesIn(): number | null {
      return closesIn();
    },
    /** How imprecise `closesIn` is (`timeout_slack_s`) — the size of the word "about". */
    get closeSlack(): number {
      return session?.timeout_slack_s ?? 30;
    },
    /** The whole idle allowance, for the sentence that explains the rule. */
    get idleTimeout(): number {
      return session?.idle_timeout_s ?? 900;
    },
    /** Is the session close enough to being handed back to be worth interrupting for? */
    get closingSoon(): boolean {
      const left = closesIn();
      return left !== null && left <= HOLD_CLOSE_WARN_S;
    },
    /** Why the session that *was* running is gone, or null. Set only for the two endings
     *  that happened *to* the user — an idle timeout, or something else ending it — never
     *  for their own "Stop and restore", which the toast already reported. */
    get ended(): SessionEnd | null {
      return ended;
    },
    /** Dismiss that notice (the user has read it, or is starting again). */
    clearEnded() {
      ended = null;
    },
    /** Whether the session state is being pushed rather than polled. Presentation only —
     *  nothing branches on it, because both channels carry the same frames. */
    get pushing() {
      return pushing;
    },

    /** Tell the daemon the user is still here, buying one whole fresh allowance.
     *
     *  **Only ever from a click.** The timeout exists so that a tab nobody is watching
     *  cannot leave a room muted, so an open socket and a status poll deliberately count
     *  for nothing — a forgotten *open* tab is the same hazard as a closed one. Calling
     *  this on a timer would restore that hazard invisibly, which is why it is a button
     *  and never an effect. */
    async stillHere() {
      if (!session?.active) return;
      try {
        adopt(await api.alignStillHere());
      } catch (e) {
        await run(() => Promise.reject(e));
      }
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
     *  daemon's own idle timeout does it — and the second of those is exactly why the state
     *  is watched over a socket rather than only polled. */
    attachSession(): () => void {
      void refresh();
      return ctl.watchSession();
    },

    /** Watch the session state — pushed, with polling as the floor. Ref-counted, because
     *  the wizard and the Outputs page both want it and there is one session.
     *
     *  Cheaper than the poll loop it replaces, not more expensive: while the socket is
     *  delivering, nothing is polled at all. That is what makes it reasonable on the
     *  Outputs page, which has to answer "are speakers held right now?" for as long as
     *  someone is looking at their speakers — and has to stop saying so the moment the
     *  hold is released. */
    watchSession(): () => void {
      watching += 1;
      if (watching === 1) {
        void pollStatus();
        openSocket();
      }
      return () => {
        watching -= 1;
        if (watching === 0) {
          if (statusTimer) {
            clearTimeout(statusTimer);
            statusTimer = null;
          }
          closeSocket();
          stopTicking();
        }
      };
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
        const started = await api.alignStartOutputs(selection, sessionModeFor(mode));
        adopt(started);
        level = started.volume;
        soloIntent = null;
        await seedOffsets();
        if (isMeasured(mode)) adopt(await api.alignAudible([], level));
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
        adopt(await api.alignAudible([nodeName], next));
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
        adopt(await api.alignAudible(nodeNames, level));
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
        adopt(await api.alignAudible([], level));
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
        adopt(await api.alignAudible([node], v));
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
      // Marks this ending as *ours*, so `adopt` resets everything derived from the
      // session without also telling the user something happened to them: `run()` below
      // has already said what happened. Everything else about the reset — the verdicts,
      // the solo, why levels and the selection survive — is `sessionClosed`'s, in one
      // place, because a session ends three ways and only one of them comes through here.
      stopping = true;
      // `alignStop` answers with the daemon's own inactive state, so there is nothing to
      // construct optimistically any more; a failed call leaves the last-known state and
      // the socket (or the next poll) says what really happened.
      let closed: AlignState | undefined;
      try {
        const ok = await run(async () => {
          closed = await api.alignStop();
          return closed;
        }, 'Alignment finished — volumes restored');
        if (ok && closed) adopt(closed);
      } finally {
        stopping = false;
        busy = false;
      }
    },

    /** Drag feedback for the volume slider — readout only, no request. */
    previewLevel(v: number) {
      level = v;
    },

    /** Commit the playback level (on slider release). */
    async setLevel(v: number) {
      level = v;
      try {
        adopt(await api.alignVolume(v));
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
      adopt(await api.alignSelect(reference, target));
    } catch (e) {
      await run(() => Promise.reject(e));
    }
  }

  return ctl;
}

export const align = createAlign();
