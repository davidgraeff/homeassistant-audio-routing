// AudioWorklet processor for microphone-assisted speaker alignment
// (docs/mic-alignment-plan.md §4.3).
//
// Runs on the audio render thread, so it must not allocate or block more than it
// has to: it fills one Int16 block, posts it to the main thread by *transferring*
// the buffer (no copy), and allocates the next one.
//
// This file is a **separate module** on purpose — a worklet cannot be inlined in
// the bundle the way a Worker blob can, so `mic.svelte.ts` imports it with Vite's
// `?url` and hands the emitted URL to `audioWorklet.addModule()`.
//
// Two decisions live here rather than on the main thread:
//
//   * **Mono, channel 0 only.** getUserMedia is asked for `channelCount: 1`, but a
//     browser that ignores that would otherwise silently change what is measured.
//     Taking channel 0 makes the stream mono whatever the browser did.
//   * **The sequence number is assigned here**, not where the block is sent. The
//     sender drops blocks under back-pressure (plan §4.3), and a number assigned
//     at send time would renumber the survivors into a gapless-looking stream —
//     exactly the silent corruption the sequence number exists to prevent.

/** Target block length in ms (~20 ms, plan §4.3). */
const BLOCK_MS = 20;

class MicCaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // `sampleRate` is a worklet global (the AudioContext's rate).
    this.size = Math.max(128, Math.round((sampleRate * BLOCK_MS) / 1000));
    this.buf = new Int16Array(this.size);
    this.fill = 0;
    this.seq = 0;
  }

  process(inputs) {
    const ch = inputs[0]?.[0];
    // No input this render quantum (the source is being connected or has gone
    // away). Returning true keeps the processor alive; the sequence number does
    // not advance, so nothing is reported as a gap for audio that never existed.
    if (!ch) return true;
    for (let i = 0; i < ch.length; i++) {
      // Clamp, then scale by 32767 so a full-scale sample lands exactly on
      // ±32767 — the two values the daemon's clip detector keys on. Rounding
      // toward zero here would hide overload instead of reporting it.
      const s = ch[i] < -1 ? -1 : ch[i] > 1 ? 1 : ch[i];
      this.buf[this.fill++] = Math.round(s * 32767);
      if (this.fill === this.size) this.flush();
    }
    return true;
  }

  flush() {
    const pcm = this.buf;
    this.buf = new Int16Array(this.size);
    this.fill = 0;
    this.port.postMessage({ seq: this.seq, pcm }, [pcm.buffer]);
    this.seq = (this.seq + 1) >>> 0; // stays a u32, as the wire format expects
  }
}

registerProcessor('mic-capture', MicCaptureProcessor);
