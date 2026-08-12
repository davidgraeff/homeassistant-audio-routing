// Microphone capture client for measurement-assisted alignment
// (docs/mic-alignment-plan.md §4).
//
// Captures the phone's microphone, batches it to ~20 ms Int16 blocks in an
// AudioWorklet and streams it to `GET /api/align/mic/ws`, which is the *only*
// consumer — the daemon does the DSP, so nothing here interprets the audio.
//
// One capture at a time (the daemon accepts one socket), so this is a module
// singleton like `align.svelte.ts` rather than per-component state.
//
// Three things are load-bearing rather than defensive:
//
//   * **Secure context first.** Without one `navigator.mediaDevices` does not
//     exist at all, and the failure has nothing to do with permissions — so it is
//     detected *before* asking for the mic and explained as the HTTPS requirement
//     it is (plan §4.1). There is no workaround.
//   * **Constraint read-back.** `echoCancellation` is designed to remove
//     loudspeaker sound from a mic signal — i.e. to remove exactly what is being
//     measured — and it adapts over seconds, so a session that starts fine decays.
//     Chrome and Safari both sometimes ignore the constraint, so the settings are
//     read back and an honoured-`true` refuses the capture (plan §4.2).
//   * **Drop, never buffer.** Under back-pressure blocks are discarded and the
//     worklet's sequence number carries the hole to the daemon, which invalidates
//     the affected window. A stale window is worse than a missing one (plan §4.3).

import { api, wsUrl } from './api';
import type { MicStatus, SignalCheck } from './types';

// @ts-ignore `?url` is a Vite import form; this project has no `vite/client`
// types reference, so TS cannot see the module's shape. Vite emits the worklet as
// its own asset and resolves this to a URL relative to the bundle (`base: './'`
// ⇒ `import.meta.url`), which is what makes it load under HA ingress — the same
// reasoning as api.ts resolving against `document.baseURI`.
//
// `no-inline` is required, not cosmetic: the worklet is under Vite's 4 kB inline
// threshold, so without it the import becomes a `data:text/javascript;base64,…`
// URL, and `audioWorklet.addModule()` on a data URL is not reliably supported
// (Firefox has rejected it outright). A worklet cannot be inlined into the bundle
// itself either — it is fetched by the audio thread, so it must stay a real file.
import workletUrl from './mic-worklet.js?url&no-inline';

/** Capture rates the daemon accepts (its `RATES`); 44.1 kHz is what iOS gives. */
const ACCEPTED_RATES = [48000, 44100];

/** Above this many bytes queued on the socket, blocks are dropped instead of
 *  buffered — ~8 blocks, i.e. ~160 ms behind. Big enough to ride out ordinary
 *  Wi-Fi jitter, small enough that what does arrive is recent. */
const MAX_BUFFERED_BYTES = 8 * 2 * Math.round((48000 * 20) / 1000);

/** How often the ingest status (level, gaps, clipping) is polled while capturing. */
const STATUS_POLL_MS = 250;

/** How much audio `GET /api/align/mic/signal` looks back over: the daemon's
 *  `PREFLIGHT_PERIODS` (2) × the 2 s click pattern.
 *
 *  Plan §12.2 asked for a pre-flight faster than the measurement gate, and the
 *  daemon now has one — `PREFLIGHT_PERIODS` is deliberately *not* the gate's
 *  `GATE_MIN_PERIODS` (4), and `align/measure.rs` has a compile-time assertion
 *  keeping them apart. Two periods is the floor rather than a preference: the
 *  estimator only keeps a period it saw whole, so the partial period at each end is
 *  dropped and a one-period window would yield none.
 *
 *  It is a *trailing* window, which is the whole reason `disturbSignal()` exists:
 *  for this long after the tone moves to another speaker or its level is dragged,
 *  the window still contains the old sound, so its verdict describes the past. A
 *  stale green is the one reading that could make a user start a run that cannot
 *  succeed, so it is withheld rather than shown.
 *
 *  Keep this in step with the daemon: too small and a stale verdict slips through,
 *  too large and every adjustment costs the user a needless wait. */
export const SIGNAL_WINDOW_MS = 4000;

/** How often the verdict is re-read while capturing. Slower than the meter: the
 *  window above slides, so polling faster only re-reports overlapping audio, and
 *  each call runs the estimator over that window on the add-on's host. */
const SIGNAL_POLL_MS = 2000;

/** Constraints §4.2 calls non-negotiable. All three processors must be off. */
const AUDIO_CONSTRAINTS: MediaTrackConstraints = {
  echoCancellation: false,
  autoGainControl: false,
  noiseSuppression: false,
  channelCount: 1,
};

/** Why the microphone cannot be used, as a **kind** and not only a sentence.
 *
 *  The kind exists because the wizard's first step reduces the mode choice from it
 *  (plan §1, §4.1, §4.2), and these are genuinely different statements: an insecure
 *  context makes measuring *impossible here* with no workaround, a denied permission is
 *  a decision the user can change, and a browser that kept echo cancellation on is a
 *  working microphone that must still be refused. A message string cannot be branched
 *  on, and inferring the kind by matching on that string is how a reworded sentence
 *  silently unlocks a mode. */
export type MicFailure =
  /** §4.1: not a secure context, so `navigator.mediaDevices` does not exist at all. */
  | 'insecure'
  /** No `AudioWorkletNode`, so nothing can be captured for measurement. */
  | 'no_worklet'
  /** The permission was refused, or the page is not allowed to ask. */
  | 'denied'
  /** No usable input device, or one that exists and could not be opened. */
  | 'no_input'
  /** §4.2: the browser kept a processor switched on despite being asked not to. */
  | 'processing'
  /** The capture rate, the worklet, or the daemon's own refusal of the stream. */
  | 'other';

/** A failure that is knowable *before* asking for the microphone. */
export interface MicBlock {
  kind: 'insecure' | 'no_worklet';
  message: string;
}

/** Why the mic cannot even be asked for, or `null` when it can be.
 *
 *  Structured rather than a bare sentence: §4.1's insecure context is the one
 *  precondition with no workaround, and the mode picker has to be able to say so
 *  differently from "this browser cannot capture". */
export function preflightBlock(): MicBlock | null {
  if (typeof navigator === 'undefined' || !navigator.mediaDevices?.getUserMedia) {
    return {
      kind: 'insecure',
      message:
        'This page is not a secure context, so the browser hides the microphone entirely. Open Home Assistant over HTTPS (not http://…:8123) and try again — there is no way around this.',
    };
  }
  if (typeof AudioWorkletNode === 'undefined') {
    return {
      kind: 'no_worklet',
      message:
        'This browser has no AudioWorklet support, so it cannot capture audio for measurement. Use a current Safari, Chrome or Firefox.',
    };
  }
  return null;
}

/** The same answer as a sentence, for callers that only display it. */
export function preflight(): string | null {
  return preflightBlock()?.message ?? null;
}

/** What the microphone check established (plan §4, and the wizard's first step).
 *
 *  One value, four states, so the mode picker never has to reason about `phase`,
 *  `error` and `caveats` separately and reach a different conclusion than the mic
 *  panel did. */
export type MicOutcomeState =
  /** Not asked for yet — and an insecure context is *not* this, because that is
   *  knowable without asking. */
  | 'unchecked'
  /** The permission prompt / worklet load is in flight. */
  | 'checking'
  /** Capturing right now; `caveats` may still carry §4.2's unreported constraints. */
  | 'working'
  | 'failed';

export interface MicOutcome {
  state: MicOutcomeState;
  /** Set for `failed` only. */
  failure: MicFailure | null;
  /** The failure's own sentence, verbatim — never re-worded by a caller. */
  message: string | null;
  /** §4.2 constraints the browser did not report either way. Only for `working`, and
   *  deliberately not a failure: Safari omits all three, and refusing on silence would
   *  rule out every iPhone. */
  caveats: string[];
}

/** Why the two **measured** modes cannot be offered, or `null` when they can
 *  (plan §1, §4.1, §4.2).
 *
 *  Returns the reason rather than a boolean because the reduction is done by
 *  *disabling with a stated reason*: a user has to be able to see that Near field
 *  exists and why it is unavailable at this moment. Short on purpose — it is rendered
 *  inside the disabled option, next to the mode's own description. */
export function measuredBlock(o: MicOutcome): string | null {
  if (o.state === 'working') return null;
  if (o.state === 'checking') return 'The microphone check has not finished yet.';
  if (o.state === 'unchecked') {
    return 'The microphone has not been checked yet. Measuring needs a live capture, so run the check on the Microphone step first.';
  }
  switch (o.failure) {
    case 'insecure':
      return 'This page is not a secure context, so the browser hides the microphone entirely. Measuring is impossible until Home Assistant is opened over HTTPS, and there is no way around it.';
    case 'no_worklet':
      return 'This browser cannot capture audio for measurement (no AudioWorklet), so there is nothing to measure with.';
    case 'denied':
      return 'The browser did not give this page the microphone, so there is nothing to measure with.';
    case 'no_input':
      return 'No usable microphone input, so there is nothing to measure with.';
    case 'processing':
      return 'The browser kept a signal processor switched on. Echo cancellation is designed to remove loudspeaker sound from a microphone signal — exactly the sound being measured — so a measured run would be wrong rather than merely noisy.';
    default:
      return 'The microphone capture did not come up, so there is nothing to measure with.';
  }
}

/** A §4.2 caveat that must travel with a measured run, or `null`.
 *
 *  Not a block: the constraints were *asked* for and the browser simply did not report
 *  whether it honoured them (Safari reports none of them). So the measured modes stay
 *  available and this sentence goes with them, rather than being dropped because it is
 *  inconvenient. */
export function measuredCaveat(o: MicOutcome): string | null {
  if (o.state !== 'working' || o.caveats.length === 0) return null;
  return `This browser did not report whether it switched off ${o.caveats.join(', ')}. Measuring is still allowed — refusing on silence would rule out every iPhone — but if a run comes out inconsistent, this is the first thing to suspect. The run itself watches for the signature (a tone that fades as the capture goes on) and says so if it sees it.`;
}

/** The processor settings that matter, as the browser actually applied them.
 *  `undefined` means the browser did not report the setting at all. */
interface AppliedSettings {
  echoCancellation?: boolean;
  autoGainControl?: boolean;
  noiseSuppression?: boolean;
  channelCount?: number;
}

/** Refusal reason if the browser kept a processor on, else `null`.
 *
 *  Only an explicit `true` refuses. A setting the browser does not report is
 *  reported as an unknown (see `unreported`) instead: Safari omits these, and
 *  refusing on silence would rule out iPhones, which are the main target. */
export function refusalFor(s: AppliedSettings): string | null {
  const on: string[] = [];
  if (s.echoCancellation === true) on.push('echo cancellation');
  if (s.autoGainControl === true) on.push('automatic gain control');
  if (s.noiseSuppression === true) on.push('noise suppression');
  if (on.length === 0) return null;
  return `The browser kept ${on.join(', ')} switched on despite being asked not to. That processing removes or reshapes the very signal being measured, so the measurement would be wrong rather than merely noisy. Try another browser, or align by ear.`;
}

/** A capture failure that knows its own kind. Thrown inside `start()` and unwrapped by
 *  its one catch, so every exit path files the same `MicFailure` the mode picker reads. */
class CaptureError extends Error {
  readonly kind: MicFailure;
  constructor(kind: MicFailure, message: string) {
    super(message);
    this.kind = kind;
  }
}

/** Classify a `getUserMedia` rejection by its `name`, which is the only part of it the
 *  spec pins down — the `message` is engine prose and differs per browser.
 *
 *  The distinction that matters to the wizard is "you said no" (a decision, changeable)
 *  versus "there is no input" (equipment) versus everything else. All three block the
 *  measured modes; they do not block them for the same reason, and telling a user to
 *  grant a permission they already granted is worse than saying nothing. */
function captureFailure(e: unknown): CaptureError {
  const name = e instanceof Error ? e.name : '';
  const own = e instanceof Error && e.message ? e.message : 'no reason given';
  switch (name) {
    case 'NotAllowedError':
    case 'SecurityError':
      return new CaptureError(
        'denied',
        'The browser did not give this page the microphone. Allow microphone access for this site (in the address-bar permission control) and start it again — or align by ear, which needs no microphone.',
      );
    case 'NotFoundError':
    case 'DevicesNotFoundError':
      return new CaptureError(
        'no_input',
        'This device reports no audio input at all, so there is nothing to capture. Plug in or enable a microphone, or align by ear.',
      );
    case 'NotReadableError':
    case 'TrackStartError':
      return new CaptureError(
        'no_input',
        `The microphone exists but could not be opened (${own}) — another application may be holding it. Close whatever is recording and try again.`,
      );
    case 'OverconstrainedError':
      return new CaptureError(
        'no_input',
        'No audio input could be opened as a single mono channel with the browser’s own processing switched off, which is what a measurement needs (plan §4.2).',
      );
    default:
      return new CaptureError('other', `The microphone could not be started: ${own}.`);
  }
}

/** Settings that were asked for but not reported back — shown as a caveat. */
export function unreported(s: AppliedSettings): string[] {
  const missing: string[] = [];
  if (s.echoCancellation === undefined) missing.push('echo cancellation');
  if (s.autoGainControl === undefined) missing.push('automatic gain control');
  if (s.noiseSuppression === undefined) missing.push('noise suppression');
  return missing;
}

function createMic() {
  let phase = $state<'idle' | 'starting' | 'capturing' | 'error'>('idle');
  let error = $state<string | null>(null);
  /** What kind of failure `error` is. Kept beside the sentence rather than derived from
   *  it, because the wizard's mode reduction branches on this. */
  let failure = $state<MicFailure | null>(null);
  /** Constraints the browser did not confirm either way (plan §4.2). */
  let caveats = $state<string[]>([]);
  /** Last ingest status from the daemon — the meter's source of truth, so a
   *  moving meter proves the whole path, not just that the browser has a mic. */
  let status = $state<MicStatus | null>(null);
  /** Whether the *level* is good enough to measure — the verdict the level meter
   *  cannot give (see `SignalCheck`). Polled here rather than in a component so the
   *  capture pre-flight and the per-speaker level setting read one value from one
   *  request instead of two components asking the same 8 s question twice. */
  let signal = $state<SignalCheck | null>(null);
  /** True while the trailing window still contains audio from before the last
   *  change, so there is deliberately no verdict to show. */
  let settling = $state(false);
  /** Blocks the sender dropped under back-pressure. The daemon sees these as
   *  sequence gaps; only this side knows they were deliberate. */
  let dropped = $state(0);
  let blocksSent = $state(0);
  let rate = $state(0);

  let ctx: AudioContext | null = null;
  let stream: MediaStream | null = null;
  let node: AudioWorkletNode | null = null;
  let source: MediaStreamAudioSourceNode | null = null;
  let ws: WebSocket | null = null;
  let poll: ReturnType<typeof setInterval> | null = null;
  let signalPoll: ReturnType<typeof setInterval> | null = null;
  /** When the thing being listened to last changed (`performance.now()`). */
  let disturbedAt = 0;

  /** Opens the socket and resolves once the daemon has accepted the hello, so a
   *  refusal (another capture already connected, unsupported rate) surfaces as a
   *  reason instead of a stream that goes nowhere. */
  function connect(sampleRate: number): Promise<WebSocket> {
    return new Promise((resolve, reject) => {
      const sock = new WebSocket(wsUrl('api/align/mic/ws'));
      sock.binaryType = 'arraybuffer';
      const timer = setTimeout(() => {
        sock.close();
        reject(new Error('the daemon did not answer the microphone connection'));
      }, 5000);
      let ready = false;
      sock.onopen = () => sock.send(JSON.stringify({ sampleRate, channelCount: 1 }));
      sock.onmessage = (ev) => {
        let msg: { type?: string; reason?: string } = {};
        try {
          msg = JSON.parse(String(ev.data));
        } catch {
          return; // not a control frame we know; ignore rather than fail
        }
        if (msg.type === 'ready') {
          ready = true;
          clearTimeout(timer);
          resolve(sock);
        } else if (msg.type === 'error') {
          clearTimeout(timer);
          const reason = msg.reason ?? 'the daemon refused the microphone stream';
          if (ready) fail(reason);
          else reject(new Error(reason));
        }
      };
      sock.onerror = () => {
        clearTimeout(timer);
        if (!ready) reject(new Error('could not open the microphone stream to the daemon'));
      };
      sock.onclose = () => {
        clearTimeout(timer);
        if (ready && phase === 'capturing') fail('the daemon closed the microphone stream');
        else if (!ready) reject(new Error('the daemon closed the microphone stream'));
      };
    });
  }

  /** A capture that was running and stopped being one. Always `other`: the stream was
   *  proven a moment ago, so this is the daemon or the network, never a permission and
   *  never a processor. */
  function fail(reason: string) {
    teardown();
    error = reason;
    failure = 'other';
    phase = 'error';
  }

  /** Releases everything, in the order that leaves nothing running: audio graph,
   *  then the device (so the browser's recording indicator goes out), then the
   *  socket. Idempotent — every exit path calls it. */
  function teardown() {
    if (poll) {
      clearInterval(poll);
      poll = null;
    }
    if (signalPoll) {
      clearInterval(signalPoll);
      signalPoll = null;
    }
    signal = null;
    settling = false;
    node?.port.close();
    node?.disconnect();
    source?.disconnect();
    node = null;
    source = null;
    stream?.getTracks().forEach((t) => t.stop());
    stream = null;
    void ctx?.close().catch(() => {});
    ctx = null;
    if (ws) {
      const sock = ws;
      ws = null; // so onclose can't recurse into fail()
      sock.onclose = null;
      sock.onerror = null;
      sock.onmessage = null;
      sock.close(1000, 'capture stopped');
    }
  }

  return {
    get phase() {
      return phase;
    },
    get error() {
      return error;
    },
    get caveats() {
      return caveats;
    },
    get status() {
      return status;
    },
    /** The level verdict, or null when there is none *yet* — either the capture has
     *  not collected a window or `settling` is withholding a stale one. Never a
     *  guess: no verdict is shown rather than an old one. */
    get signal() {
      return signal;
    },
    /** True while a verdict is deliberately withheld because what the microphone is
     *  listening to changed less than `SIGNAL_WINDOW_MS` ago. */
    get signalSettling() {
      return settling;
    },
    get dropped() {
      return dropped;
    },
    get blocksSent() {
      return blocksSent;
    },
    /** The AudioContext's rate — what the daemon was told to expect. */
    get rate() {
      return rate;
    },
    get preflightError() {
      return preflight();
    },
    /** What the check has established, in one value (see `MicOutcome`).
     *
     *  Composed here rather than in the wizard so the mic panel and the mode picker
     *  cannot reach different conclusions from the same three fields. Two properties are
     *  load-bearing:
     *
     *  * an **insecure context is `failed` before anything is asked for** — §4.1 is
     *    detectable without a permission prompt, and prompting for a microphone the
     *    browser has already hidden would be theatre;
     *  * `working` means **capturing right now**, not "worked once". The measured modes
     *    need a live capture (the daemon refuses with `mic_missing` otherwise), so a
     *    capture the user has stopped must not leave a mode enabled that cannot run. */
    get outcome(): MicOutcome {
      const blocked = preflightBlock();
      if (blocked) return { state: 'failed', failure: blocked.kind, message: blocked.message, caveats: [] };
      if (phase === 'capturing') return { state: 'working', failure: null, message: null, caveats };
      if (phase === 'starting') return { state: 'checking', failure: null, message: null, caveats: [] };
      if (phase === 'error') return { state: 'failed', failure: failure ?? 'other', message: error, caveats: [] };
      return { state: 'unchecked', failure: null, message: null, caveats: [] };
    },

    /** Ask for the mic and start streaming. Must be called from a user gesture:
     *  both the permission prompt and (on iOS) resuming the AudioContext need one. */
    async start() {
      if (phase === 'starting' || phase === 'capturing') return;
      error = null;
      failure = null;
      caveats = [];
      dropped = 0;
      blocksSent = 0;
      status = null;
      const blocked = preflightBlock();
      if (blocked) {
        error = blocked.message;
        failure = blocked.kind;
        phase = 'error';
        return;
      }
      phase = 'starting';
      try {
        try {
          stream = await navigator.mediaDevices.getUserMedia({ audio: AUDIO_CONSTRAINTS, video: false });
        } catch (e) {
          throw captureFailure(e);
        }
        const track = stream.getAudioTracks()[0];
        if (!track) throw new CaptureError('no_input', 'the browser returned a stream with no audio track');
        const settings = track.getSettings() as AppliedSettings;
        const refusal = refusalFor(settings);
        // §4.2's one hard refusal, and it is a *processing* failure rather than a broken
        // microphone: the capture works perfectly and would measure the wrong thing.
        if (refusal) throw new CaptureError('processing', refusal);
        caveats = unreported(settings);

        ctx = new AudioContext();
        rate = ctx.sampleRate;
        if (!ACCEPTED_RATES.includes(rate)) {
          throw new CaptureError(
            'other',
            `This browser captures at ${rate} Hz; the daemon accepts 48000 or 44100 Hz. Resampling in the browser would add an unknown delay to what is being measured, so this capture is refused.`,
          );
        }
        await loadWorklet(ctx);
        await ctx.resume();

        ws = await connect(rate);
        node = new AudioWorkletNode(ctx, 'mic-capture', { numberOfInputs: 1, numberOfOutputs: 0 });
        node.port.onmessage = (ev: MessageEvent<{ seq: number; pcm: Int16Array }>) => send(ev.data.seq, ev.data.pcm);
        source = ctx.createMediaStreamSource(stream);
        source.connect(node);

        phase = 'capturing';
        // A capture that just started has nothing to judge yet, and the same
        // withholding rule covers it: no verdict until a full window is this
        // capture's own.
        disturbedAt = performance.now();
        settling = true;
        poll = setInterval(() => {
          void api
            .micStatus()
            .then((s) => {
              status = s;
            })
            .catch(() => {});
        }, STATUS_POLL_MS);
        signalPoll = setInterval(() => {
          void api
            .micSignal()
            .then((s) => {
              // Drop anything whose window overlaps the last change: it is a verdict
              // about the previous speaker or the previous level.
              if (performance.now() - disturbedAt < SIGNAL_WINDOW_MS) return;
              signal = s;
              settling = false;
            })
            .catch(() => {});
        }, SIGNAL_POLL_MS);
      } catch (e) {
        teardown();
        error = e instanceof Error ? e.message : String(e);
        failure = e instanceof CaptureError ? e.kind : 'other';
        phase = 'error';
      }
    },

    /** Say that what the microphone is listening to has just changed — the tone
     *  moved to another speaker, or its level was dragged.
     *
     *  Withholds the level verdict for one window rather than letting the caller show
     *  a reading taken at the *old* level. That is not caution: a stale green is the
     *  one wrong answer here that would send a user into a run that cannot succeed. */
    disturbSignal() {
      disturbedAt = performance.now();
      signal = null;
      settling = true;
    },

    /** Stop capturing. Leaves the alignment session alone — the daemon treats a
     *  closed mic socket as "ingest gone", not "session over". */
    stop() {
      teardown();
      phase = 'idle';
      error = null;
      failure = null;
    },
  };

  /** One block onto the wire: `[u32 LE seq][Int16LE samples]`. */
  function send(seq: number, pcm: Int16Array) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    // Back-pressure: drop, never queue. The worklet already owns the sequence
    // number, so the daemon sees the hole and discards the window it spans
    // (plan §4.3) instead of measuring stale audio.
    if (ws.bufferedAmount > MAX_BUFFERED_BYTES) {
      dropped += 1;
      return;
    }
    const frame = new Uint8Array(4 + pcm.byteLength);
    new DataView(frame.buffer).setUint32(0, seq, true);
    frame.set(new Uint8Array(pcm.buffer, pcm.byteOffset, pcm.byteLength), 4);
    ws.send(frame);
    blocksSent += 1;
  }
}


/** Load the AudioWorklet module, with a diagnosis and a fallback.
 *
 *  `addModule()` is deliberately opaque: Chrome rejects with
 *  `AbortError: Unable to load a worklet's module.` whether the fetch 404'd, the MIME
 *  type was refused, a Content-Security-Policy blocked it, or the script threw while
 *  evaluating. That single sentence is what a user sees, and it names none of them.
 *
 *  Observed on a real deployment behind Home Assistant ingress: the asset is served
 *  correctly (200, `text/javascript`) and `addModule` still failed — so the message has
 *  to distinguish "never arrived" from "arrived and was rejected", or there is nothing to
 *  act on.
 *
 *  So: fetch it ourselves first to establish which half failed, then try the URL, then
 *  fall back to a **blob** URL built from the source we just fetched. The blob is
 *  same-origin and bypasses whatever the proxy does to the response, which is the standard
 *  workaround for this class of failure. (A `data:` URL is *not* — Firefox rejects those,
 *  which is why the import above needs `no-inline`.)
 */
async function loadWorklet(ctx: AudioContext): Promise<void> {
  const url = String(workletUrl);

  let source: string | null = null;
  let probe = '';
  try {
    const resp = await fetch(url, { credentials: 'same-origin' });
    probe = `HTTP ${resp.status} ${resp.statusText}, type ${resp.headers.get('content-type') ?? 'none'}`;
    if (resp.ok) source = await resp.text();
  } catch (e) {
    probe = `the request itself failed: ${e instanceof Error ? e.message : String(e)}`;
  }

  // `credentials` is the documented knob and defaults vary by engine; being explicit
  // costs nothing and removes one variable from the diagnosis.
  try {
    await ctx.audioWorklet.addModule(url, { credentials: 'same-origin' });
    return;
  } catch (first) {
    if (source === null) {
      throw new Error(
        `could not load the audio worklet from ${url} — and fetching it directly did not work either (${probe}). ` +
          `That points at the build or the way the add-on serves its assets, not at your microphone.`,
      );
    }
    // It is there and readable, so this is the browser refusing the *response* — a MIME
    // type it will not accept for a worklet, or a CSP. Serving the same bytes from a blob
    // sidesteps both.
    const blob = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
    try {
      await ctx.audioWorklet.addModule(blob, { credentials: 'same-origin' });
      return;
    } catch (second) {
      const why = (e: unknown) => (e instanceof Error ? e.message : String(e));
      throw new Error(
        `could not load the audio worklet. The file is reachable (${probe}), so this is not a missing asset: ` +
          `loading it from ${url} failed with "${why(first)}", and loading the same bytes from a blob URL failed with ` +
          `"${why(second)}". That leaves the script being rejected on evaluation — try another browser and report both messages.`,
      );
    } finally {
      URL.revokeObjectURL(blob);
    }
  }
}

export const mic = createMic();
