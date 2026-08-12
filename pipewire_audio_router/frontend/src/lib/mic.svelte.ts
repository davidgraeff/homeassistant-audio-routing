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

/** Why the mic cannot be used at all, or `null` when it can be asked for. */
export function preflight(): string | null {
  if (typeof navigator === 'undefined' || !navigator.mediaDevices?.getUserMedia) {
    return 'This page is not a secure context, so the browser hides the microphone entirely. Open Home Assistant over HTTPS (not http://…:8123) and try again — there is no way around this.';
  }
  if (typeof AudioWorkletNode === 'undefined') {
    return 'This browser has no AudioWorklet support, so it cannot capture audio for measurement. Use a current Safari, Chrome or Firefox.';
  }
  return null;
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

  function fail(reason: string) {
    teardown();
    error = reason;
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

    /** Ask for the mic and start streaming. Must be called from a user gesture:
     *  both the permission prompt and (on iOS) resuming the AudioContext need one. */
    async start() {
      if (phase === 'starting' || phase === 'capturing') return;
      error = null;
      caveats = [];
      dropped = 0;
      blocksSent = 0;
      status = null;
      const blocked = preflight();
      if (blocked) {
        error = blocked;
        phase = 'error';
        return;
      }
      phase = 'starting';
      try {
        stream = await navigator.mediaDevices.getUserMedia({ audio: AUDIO_CONSTRAINTS, video: false });
        const track = stream.getAudioTracks()[0];
        if (!track) throw new Error('the browser returned a stream with no audio track');
        const settings = track.getSettings() as AppliedSettings;
        const refusal = refusalFor(settings);
        if (refusal) throw new Error(refusal);
        caveats = unreported(settings);

        ctx = new AudioContext();
        rate = ctx.sampleRate;
        if (!ACCEPTED_RATES.includes(rate)) {
          throw new Error(
            `This browser captures at ${rate} Hz; the daemon accepts 48000 or 44100 Hz. Resampling in the browser would add an unknown delay to what is being measured, so this capture is refused.`,
          );
        }
        // A worklet 404 is the one failure that means the *build* is wrong rather
        // than the device, so say which URL failed.
        try {
          await ctx.audioWorklet.addModule(workletUrl);
        } catch (e) {
          throw new Error(`could not load the audio worklet from ${String(workletUrl)}: ${e instanceof Error ? e.message : String(e)}`);
        }
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

export const mic = createMic();
