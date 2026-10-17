/**
 * How a source and its senders read: kind, sender label, "last seen", peak percent.
 *
 * Pure display formatting, kept out of the tab so the component is about the form and
 * the API calls. A remembered AirPlay sender may have no name and no address yet — only
 * the key the daemon tracks it by — so the label falls back rather than rendering blank.
 */
import type { AirplayClient, SourceKind, SpotifyStatus } from '../types';

export function kindLabel(kind: SourceKind): string {
  switch (kind) {
    case 'airplay':
      return 'AirPlay';
    case 'spotify':
      return 'Spotify';
    case 'rtp':
      return 'RTP';
  }
}

/** One line on who is playing on a Spotify device, or null when nobody is. */
export function spotifyListener(st: SpotifyStatus | null | undefined): string | null {
  if (!st?.user) return null;
  return st.client ? `${st.user} · ${st.client}` : st.user;
}

export function clientLabel(c: AirplayClient): string {
  return c.name ?? c.addr ?? c.key;
}

export function formatWhen(unixSecs: number): string {
  if (!unixSecs) return 'never';
  return new Date(unixSecs * 1000).toLocaleString();
}

/** A source's peak (0.0–1.0) as a bar width. */
export const pct = (peak: number) => Math.min(100, Math.round(peak * 100));
