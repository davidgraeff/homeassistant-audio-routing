// Microphone-assisted alignment: the *run*, as the UI sees it
// (docs/mic-alignment-plan.md §§8-11, bridge-daemon/src/align_measure.rs).
//
// The daemon runs one measurement at a time, process-wide — the same shape as the
// by-ear session in `align.svelte.ts` — so this is a module singleton rather than
// component state. What it owns is the poll loop and the four actions; the wizard
// components own where the user is in the flow.
//
// Two things here are deliberate rather than incidental:
//
//   * **Refusals keep their structure.** `api.refusalOf()` recovers the whole
//     `Refusal` from a rejected call, so the UI can name the kind, the blamed
//     member and the estimator's own verdict. The plan spent its effort on a
//     daemon that refuses rather than guesses (§5.5); rendering that as a generic
//     error would throw the effort away.
//   * **Nothing here interprets the numbers.** Confidence bands and labels below
//     are presentation only. Whether a proposal may be written is `can_apply` and
//     `proposal.blocked`, both decided by the daemon — a client-side opinion about
//     a good-enough standard error would be a second, quieter policy.

import { MEASURE_WS_PATH, api, refusalOf, wsUrl } from './api';
import { toast } from './toast';
import type {
  GateReason,
  MeasureMode,
  MeasurePhase,
  MeasureStatus,
  Refusal,
  RefusalKind,
  EstimatorReason,
  WarningKind,
} from './types';

/** Phases in which the daemon is doing something on its own — poll fast. */
const LIVE_PHASES: ReadonlySet<MeasurePhase> = new Set<MeasurePhase>([
  'arming',
  'learning',
  'measuring',
  'solving',
  'writing',
  'settling',
  'verifying',
]);

/** While a run is live. A measurement window is seconds long, so this is about
 *  keeping the gate readout current, not about latency. */
const POLL_LIVE_MS = 1000;
/** Parked (proposed / done / refused / idle): still polled, because a *second*
 *  browser or the HA integration can move the run, but slowly. */
const POLL_IDLE_MS = 5000;

/** How long to wait before trying the push socket again after it failed or dropped.
 *  Polling continues throughout, so this is a quiet upgrade attempt, not a retry the
 *  user is waiting on. */
const WS_RETRY_MS = 15000;

/** Is the daemon working on this run right now? */
export function isLive(phase: MeasurePhase): boolean {
  return LIVE_PHASES.has(phase);
}

/** The §8 state machine, in the order the run walks it — the spine of the run
 *  page's progress display. `refused` is not in it: it can replace any of these. */
export const PHASE_CHAIN: readonly MeasurePhase[] = [
  'arming',
  'learning',
  'measuring',
  'solving',
  'proposed',
  'writing',
  'settling',
  'verifying',
  'done',
];

const PHASE_LABELS: Record<MeasurePhase, string> = {
  idle: 'Not measuring',
  arming: 'Arming',
  learning: 'Learning levels',
  measuring: 'Measuring',
  solving: 'Solving',
  proposed: 'Proposal ready',
  writing: 'Writing the knobs',
  settling: 'Waiting for speakers',
  verifying: 'Verifying',
  done: 'Finished',
  refused: 'Stopped',
};

export function phaseLabel(phase: MeasurePhase): string {
  return PHASE_LABELS[phase];
}

/** Why a phase can take a long time. Shown beside the daemon's own message,
 *  which says what it is doing *now*; this says what to expect. */
const PHASE_NOTES: Partial<Record<MeasurePhase, string>> = {
  arming: 'Checking the session, the capture and the timing lock before anything is measured.',
  learning: 'Finding a playback level each speaker can be heard at over the room.',
  measuring:
    'Each speaker is soloed in turn and measured twice. Every switch waits for the tone to settle, so this is the long part — about 11 seconds per speaker per pass.',
  solving: "Turning the arrivals into a setting for each speaker's own timing knob.",
  writing: "Writing each speaker's knob — an advance for Sendspin, a delay for AirPlay 2 and PipeWire hosts.",
  settling:
    'Speakers reconnect when their knob changes, and a reconnecting speaker can take tens of seconds to render again. Nothing is wrong while this waits.',
  verifying: 'Re-measuring what was written, to check it landed where it was supposed to.',
};

export function phaseNote(phase: MeasurePhase): string | null {
  return PHASE_NOTES[phase] ?? null;
}

/** What the gate is waiting for. These are the "why is nothing happening"
 *  answers — mute settling, a reconnect, the phone moving, a doorbell. */
const GATE_REASONS: Record<GateReason, string> = {
  mic_disconnected: 'the microphone stream is not connected',
  mic_reconnected: 'the microphone reconnected, which restarts the timing reference',
  sequence_gap: 'audio blocks went missing between the phone and the add-on',
  clipped: 'the capture is clipping — turn the playback level down',
  silent: 'nothing is arriving from this speaker yet',
  interference: 'an announcement or a voice-assistant turn is playing on this speaker — it outranks the alignment',
  intermittent: "this speaker's stream keeps breaking up, so its tone comes and goes",
  unstable_amplitude: 'the level keeps changing — hold the phone still',
  aec_suspected: 'the browser looks like it is cancelling the tone it is meant to hear',
  acquiring: 'collecting pattern repeats',
  estimator: 'the estimator is not satisfied with what it has yet',
};

export function gateReasonLabel(reason: GateReason): string {
  return GATE_REASONS[reason];
}

/** A short headline per refusal kind. The daemon's `message` is the sentence the
 *  user acts on and is always shown verbatim next to this. */
const REFUSAL_KINDS: Record<RefusalKind, string> = {
  no_session: 'No alignment session is running',
  session_lost: 'The alignment session stopped',
  session_changed: 'The alignment session moved to another group',
  mic_missing: 'No microphone is connected',
  mic_lost: 'The microphone went away',
  mic_reconnected: 'The microphone reconnected too often',
  mode_unsupported: 'That mode is not available yet',
  estimator: 'The estimator refused to answer',
  gate_timeout: 'Never got a stable signal',
  interference: 'Something more important played on a speaker',
  ambiguous_spread: 'The arrivals are too far apart to be read unambiguously',
  transitivity: 'The cross-band check failed',
  repeatability: 'The two passes disagreed',
  knob_range: 'These speakers cannot be made to arrive together',
  residual_too_large: 'Still not aligned after the write',
  write_failed: 'Writing a speaker’s knob failed',
  cancelled: 'Stopped',
  internal: 'Cannot run',
};

export function refusalKindLabel(kind: RefusalKind): string {
  return REFUSAL_KINDS[kind];
}

/** The estimator's own verdict, in the user's terms. */
const ESTIMATOR_REASONS: Record<EstimatorReason, string> = {
  low_snr: 'the tone was not far enough above the room noise',
  ambiguous_peak: 'it could not tell which arrival was the direct sound',
  unstable_phase: 'the arrival moved between pattern repeats',
  clipped: 'the capture clipped',
  sequence_gap: 'audio blocks went missing from the capture',
  too_few_periods: 'too few pattern repeats were usable',
};

export function estimatorReasonLabel(reason: EstimatorReason): string {
  return ESTIMATOR_REASONS[reason];
}

const WARNING_KINDS: Record<WarningKind, string> = {
  send_ahead_high_water: 'Raises the whole group’s buffer',
  aec_suspected: 'Echo cancellation suspected',
  level_learning_skipped: 'One level was used for every speaker',
  mic_reconnected: 'The microphone reconnected',
  no_drift_fit: 'Clock drift could not be fitted',
  interference: 'Something else played during the run',
};

export function warningKindLabel(kind: WarningKind): string {
  return WARNING_KINDS[kind];
}

export const MODE_LABELS: Record<MeasureMode, string> = {
  sweet_spot: 'Multi-position',
  near_field: 'Near field',
};

/** How firm one member's number is, from its standard error.
 *
 *  Bands are for reading, not for deciding: the estimator already refuses above
 *  1 ms (`MAX_STD_ERROR_MS`), so everything that reaches the UI is inside its own
 *  limit. The point of showing this is that a 9 ms delta at 0.05 ms and one at
 *  0.9 ms are not the same claim, and a table of bare milliseconds hides that. */
export function confidenceBand(stdErrorMs: number): 'tight' | 'good' | 'soft' {
  if (stdErrorMs <= 0.1) return 'tight';
  if (stdErrorMs <= 0.3) return 'good';
  return 'soft';
}

/** Set when the uncertainty is a large share of the correction being proposed —
 *  i.e. when the *sign* of the change is solid but its size is not. */
export function uncertaintyDominates(addedMs: number, stdErrorMs: number): boolean {
  const size = Math.abs(addedMs);
  return size > 0 && stdErrorMs >= size * 0.25;
}

/** `elapsed_s` as m:ss — a run is minutes long (plan §8's budget), so seconds
 *  alone stop being readable about a minute in. */
export function elapsed(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${String(s).padStart(2, '0')}`;
}

function createMeasure() {
  let status = $state<MeasureStatus | null>(null);
  let busy = $state(false);
  /** The refusal an *action* was rejected with (start/apply/revert). Distinct from
   *  `status.refusal`, which is how a run that had already started ended. */
  let actionRefusal = $state<Refusal | null>(null);
  /** Non-refusal failure of an action (network, a 500) — kept separately so it is
   *  never dressed up as a refusal the daemon reasoned about. */
  let actionError = $state<string | null>(null);
  let timer: ReturnType<typeof setTimeout> | null = null;
  let mounted = 0;

  // ---- The push channel, and why polling never goes away ---------------------
  //
  // Plan §11 wants progress pushed rather than polled, and `GET
  // /api/align/measure/ws` sends one whole status on connect and one per change. But
  // a wizard that shows nothing because a route 404s or a proxy refuses the upgrade
  // is worse than one that polls, so the two run in this order:
  //
  //   * polling starts immediately on mount and keeps going;
  //   * the socket only *earns* the right to stop it by delivering an actual status —
  //     an upgrade that succeeds and then says nothing (a proxy that holds the
  //     connection open) therefore changes nothing;
  //   * a close or an error puts polling back and re-tries the socket later.
  let socket: WebSocket | null = null;
  let retry: ReturnType<typeof setTimeout> | null = null;
  /** True once the socket has actually delivered a status, i.e. polling is off. */
  let pushing = $state(false);

  function adopt(s: MeasureStatus) {
    status = s;
  }

  async function poll() {
    try {
      adopt(await api.measureStatus());
    } catch {
      /* keep the last-known status: a failed poll is not a state change */
    }
    schedule();
  }

  function schedule() {
    if (timer) clearTimeout(timer);
    timer =
      mounted > 0 && !pushing
        ? setTimeout(() => void poll(), status && isLive(status.phase) ? POLL_LIVE_MS : POLL_IDLE_MS)
        : null;
  }

  function scheduleRetry() {
    if (retry || mounted === 0) return;
    retry = setTimeout(() => {
      retry = null;
      openSocket();
    }, WS_RETRY_MS);
  }

  function openSocket() {
    if (socket || mounted === 0) return;
    let sock: WebSocket;
    try {
      sock = new WebSocket(wsUrl(MEASURE_WS_PATH));
    } catch {
      scheduleRetry();
      return;
    }
    socket = sock;
    sock.onmessage = (ev) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(String(ev.data));
      } catch {
        return; // not a status frame; polling is still running if we never adopted one
      }
      // Shape-checked rather than trusted: adopting `{}` would blank the whole
      // wizard, and the fallback exists precisely because this route may not be the
      // one we think it is yet.
      if (!parsed || typeof parsed !== 'object' || typeof (parsed as MeasureStatus).phase !== 'string') return;
      adopt(parsed as MeasureStatus);
      if (!pushing) {
        pushing = true;
        if (timer) {
          clearTimeout(timer);
          timer = null;
        }
      }
    };
    sock.onclose = () => {
      if (socket !== sock) return;
      socket = null;
      pushing = false;
      // Straight back to polling, then a quiet attempt to get the socket back.
      void poll();
      scheduleRetry();
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
    sock.close(1000, 'wizard closed');
  }

  /** The speakers a pending revert belongs to. `revert_scope` is the contract; the
   *  `sources` fallback covers a daemon that predates the field. */
  function revertScope(): string[] {
    return status?.revert_scope ?? status?.sources ?? [];
  }

  /** Run an action, keeping a refusal's structure. Returns the new status, or
   *  null when it was refused. */
  async function act(call: () => Promise<MeasureStatus>): Promise<MeasureStatus | null> {
    busy = true;
    actionRefusal = null;
    actionError = null;
    try {
      const s = await call();
      adopt(s);
      schedule();
      return s;
    } catch (e) {
      const refusal = refusalOf(e);
      if (refusal) {
        actionRefusal = refusal;
        toast('error', refusal.message);
      } else {
        actionError = e instanceof Error ? e.message : String(e);
        toast('error', actionError);
      }
      // Even a refused action can have moved the daemon (apply refuses *after*
      // deciding), so re-read rather than trusting the local copy.
      void poll();
      return null;
    } finally {
      busy = false;
    }
  }

  return {
    get status() {
      return status;
    },
    get busy() {
      return busy;
    },
    get actionRefusal() {
      return actionRefusal;
    },
    get actionError() {
      return actionError;
    },
    get phase(): MeasurePhase {
      return status?.phase ?? 'idle';
    },
    /** Is the daemon working right now? */
    get live() {
      return !!status && isLive(status.phase);
    },
    get canApply() {
      return !!status?.can_apply;
    },
    get canRevert() {
      return !!status?.can_revert;
    },
    /** Whether the run is being pushed rather than polled. Presentation only —
     *  nothing branches on it, because both paths carry the same status. */
    get pushing() {
      return pushing;
    },

    /** The speakers a pending revert would put back (`revert_scope`, plan §9.4).
     *
     *  The daemon retains it across `abandon()`, which is what lets the offer survive
     *  a page reload — a client-side memory of the last run could not. The `sources`
     *  fallback covers only a daemon older than the field: the offer then disappears
     *  once the run is abandoned, which is honest about what is knowable rather than
     *  guessed. */
    get revertScope(): string[] {
      return revertScope();
    },

    /** Whether a revertable write touches any of these speakers.
     *
     *  The overlap form, not the exact-set form, is what a *panel* needs: a wizard run
     *  is scoped to a selection of outputs (plan §12.3.1), so its scope will rarely be
     *  identical to any one source group's — but a delay written to a speaker that
     *  group contains is still that group's problem, and the undo has to be reachable
     *  from where the user notices it. */
    revertTouches(nodeNames: string[]): boolean {
      const scope = new Set(revertScope());
      return nodeNames.some((n) => scope.has(n));
    },

    /** Mount hook: watch the run while anything is showing it. Ref-counted, because
     *  the panel and the wizard both want it and there is only one run.
     *
     *  Polls from the first moment and prefers the socket only once it has proven
     *  itself — see the comment above `openSocket`. */
    attach(): () => void {
      mounted += 1;
      if (mounted === 1) {
        void poll();
        openSocket();
      }
      return () => {
        mounted -= 1;
        if (mounted === 0) {
          if (timer) {
            clearTimeout(timer);
            timer = null;
          }
          closeSocket();
        }
      };
    },

    /** Clear the last action's error, e.g. when the user changes something. */
    clearError() {
      actionRefusal = null;
      actionError = null;
    },

    start: (mode: MeasureMode) => act(() => api.measureStart(mode)),
    apply: () => act(() => api.measureApply()),
    revert: () => act(() => api.measureRevert()),
    abandon: () => act(() => api.measureAbandon()),
  };
}

export const measure = createMeasure();
