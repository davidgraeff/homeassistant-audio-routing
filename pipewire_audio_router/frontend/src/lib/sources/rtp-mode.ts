/**
 * The RTP source's "Source" mode: one choice in the UI, two knobs on the daemon.
 *
 * `source.ip` and `sess.ignore-ssrc` are what the receiver actually takes, and two of
 * the three modes differ only in the second — so presenting them as two checkboxes
 * made "accept all senders" and "only one client" look unrelated when they are the
 * same decision. This maps between the one choice and the two knobs, in both
 * directions, so the form and the stored config cannot disagree about what a config
 * means.
 */
export type RtpMode = 'all' | 'multicast' | 'single';

/** The default group when a user picks multicast without typing an address. */
export const DEFAULT_MULTICAST_ADDR = '239.255.42.42';

export const RTP_MODES: { value: RtpMode; label: string; desc: string }[] = [
  {
    value: 'all',
    label: 'Accept all senders',
    desc: 'Any device sending to this port is received. Packets from two senders may interleave and corrupt the audio.',
  },
  {
    value: 'single',
    label: 'Only one client',
    desc: "Locks onto the first sender's stream and rejects all others — the corruption guard. Needs firmware with a stable SSRC (any recent bt-bridge build).",
  },
  {
    value: 'multicast',
    label: 'Multicast group',
    desc: 'Join a group so several boxes share one stream. Set the same group on every receiver and point the firmware’s RTP host at it. IPv4 or IPv6.',
  },
];

/**
 * A non-empty, non-`0.0.0.0` `source.ip` means the receiver joined a multicast group;
 * otherwise `ignore-ssrc` distinguishes "accept all" from "single".
 */
export function deriveRtpMode(addr: string, ignoreSsrc: boolean): RtpMode {
  if (addr && addr !== '0.0.0.0') return 'multicast';
  return ignoreSsrc ? 'all' : 'single';
}

/** The inverse: the two backend knobs for a chosen mode. */
export function rtpModeToParams(mode: RtpMode, multicastAddr: string): { sourceAddr: string; ignoreSsrc: boolean } {
  if (mode === 'multicast') return { sourceAddr: multicastAddr.trim() || DEFAULT_MULTICAST_ADDR, ignoreSsrc: true };
  if (mode === 'single') return { sourceAddr: '0.0.0.0', ignoreSsrc: false };
  return { sourceAddr: '0.0.0.0', ignoreSsrc: true };
}
