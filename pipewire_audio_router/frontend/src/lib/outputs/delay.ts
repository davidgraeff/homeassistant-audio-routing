/**
 * The delay knob's three shapes, one per output kind.
 *
 * One gesture — "this speaker is out of step, move it in time" — over three
 * mechanisms: AirPlay 2 shifts its render delay, a PipeWire host sets its receiver's
 * jitter buffer, a sendspin speaker takes a static trim against its group. The scales
 * differ by an order of magnitude and so does the cost of applying one, which is what
 * `liveDuringDrag` encodes.
 *
 * Data and pure functions only, so the numbers and the sentences that explain them can
 * be read without the component around them.
 */
import type { OutputInfo } from '../types';

// One delay slider, three kinds. AirPlay 2 shifts its render delay, a PipeWire host
// sets its receiver's jitter buffer, a sendspin speaker takes a static trim against
// its group — different mechanisms, one gesture: "this speaker is out of step, move it
// in time". The scales differ by an order of magnitude, so each kind brings its own
// spec.
//
// Defaults are not hardcoded here: the daemon reports what each output is actually
// running, as `latency_effective_ms`.
export type DelaySpec = {
  min: number;
  max: number;
  step: number;
  /** Below this the low end is dangerous. **0 disables the risky zone** — for a
   *  knob whose zero simply means "no adjustment", low is not a risk at all. */
  riskyBelow: number;
  highAbove: number;
  label: string;
  /** Sentence for the low end; only used when `riskyBelow > 0`. */
  risk: string;
  /** Sentence for the healthy middle, in this knob's own terms. */
  good: string;
  /** Push intermediate values *while* dragging, so the knob can be found by ear?
   *  Only where applying one is cheap and gapless. */
  liveDuringDrag: boolean;
  /** Is there an *override* to drop (→ a "Default" button)? False for a knob whose
   *  neutral position is 0, which the slider can already reach. */
  clearable: boolean;
  /** Which endpoint holds this value: the shared per-output latency, or sendspin's
   *  own static-delay store. */
  store: 'latency' | 'sendspin';
};
export const AP2_DELAY: DelaySpec = {
  min: 0,
  max: 2000,
  step: 10,
  // The daemon shifts the next PT=87 anchor on the running stream: no reconnect and
  // nothing to reload, so hearing the drag is worth the throttled round trips.
  liveDuringDrag: true,
  // 0 is the normal position — the receivers render from the anchors as they arrive —
  // so the low end carries no warning. A receiver that goes *silent* needs more delay
  // rather than less, which its fault badge reports; it is not a slider zone.
  riskyBelow: 0,
  // Above this it plays fine, but the delay is latency the whole system pays for.
  highAbove: 200,
  label: 'Render delay',
  risk: '',
  good: 'Shifts when this receiver renders, relative to the rest of its group',
  clearable: true,
  store: 'latency',
};
export const PWSINK_DELAY: DelaySpec = {
  // Three packet times (sync_settings::PWSINK_JITTER_MIN_MS): the receiving module
  // refuses a buffer below its packet time, and the sender needs room for a
  // catch-up burst *inside* the buffer. 0 is not on offer here the way it is for
  // AP2 — the daemon would clamp it anyway.
  min: 15,
  // Well short of what the API accepts (2000 ms), but far enough to line a remote
  // host up against a slow receiver rather than only to tune its own buffer. The
  // zone marks below stay where they are: past `highAbove` this is still latency
  // you are choosing to add, which is exactly what aligning against something slow
  // means, and the note says so rather than warning you off.
  max: 800,
  step: 5, // a whole number of 5 ms packets, which is what the receiver wants
  // Four packet times: below that the receiver holds under a packet of slack for
  // network or scheduling jitter.
  riskyBelow: 20,
  highAbove: 300,
  label: 'Playout delay',
  risk: 'the receiving host holds under a packet of slack for network or scheduling jitter — expect crackles',
  // Applying one reloads `module-rtp-session` on the remote host — a short gap in
  // that speaker's audio. Doing that at every step of a drag would be a stutter,
  // so this kind commits only when you let go.
  liveDuringDrag: false,
  good: "Buffers the network hop and the remote host's scheduling",
  clearable: true,
  store: 'latency',
};
export const SENDSPIN_DELAY: DelaySpec = {
  // 0 is the *normal* position here, not a risky one: this knob trims a speaker
  // that is consistently early against the rest of its group, so "no trim" is the
  // resting state and there is no low-end risk zone at all.
  min: 0,
  // The daemon clamps a static delay at 5000 ms, so the slider reaches everything
  // the API accepts. A tighter ceiling would make an already-stored larger value
  // unreachable — and silently shrink it on the next drag.
  max: 5000,
  step: 10,
  riskyBelow: 0,
  highAbove: 1000,
  label: 'Static delay',
  risk: '',
  good: 'Trims this speaker against the rest of its group',
  // The change is in-band, but whether the *firmware* applies it live is a
  // per-setup question (Settings → sendspin delay applied live). Where it does not,
  // every change reconnects that speaker, which costs tens of seconds of silence —
  // so this commits on release, which is right either way.
  liveDuringDrag: false,
  clearable: false,
  store: 'sendspin',
};
export const delaySpec = (o: OutputInfo) =>
  o.kind === 'pwsink' ? PWSINK_DELAY : o.kind === 'sendspin' ? SENDSPIN_DELAY : AP2_DELAY;
/** Every adopted kind has one now; still a predicate, so the row keeps its guard. */
export const hasDelayKnob = (o: OutputInfo) => o.kind === 'airplay2' || o.kind === 'pwsink' || o.kind === 'sendspin';
