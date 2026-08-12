/**
 * What an output *reads* as: its kind, its status and PTP badges, and whether a test
 * clip can be played to it.
 *
 * Every function here is `OutputInfo` in, display value out — no component state, no
 * API calls. The long comments are the reason it is worth having: each badge encodes a
 * distinction that took a bug to find (reachable is not connected; an AirPlay-2
 * receiver without a PTP lock is fine alone and only drifts in a group; a sendspin
 * speaker cannot be tested before it is added).
 */
import type { OutputInfo } from '../types';

export function kindLabel(o: OutputInfo): string {
  if (o.kind === 'sendspin') return 'Sendspin';
  if (o.kind === 'pwsink') return 'PipeWire';
  return 'AirPlay 2';
}

// Status badge, three-state: offline (not reachable) / not connected (no session) /
// online. `present` is reachability only — mDNS presence as owned by the liveness
// tasks. For pw-sink that really is all it means: the remote host advertises over
// mDNS, but audio only flows once its module-rtp-session initiates the AppleMIDI
// handshake (receiver-driven), so reachable-but-unattached is a genuine third
// state. Folding it into "offline" (as this page used to) is what made it
// disagree with the routing graph about the same target — the graph read
// presence, this page read the session. Now both show all three states.
//
// Only an *added* pw-sink target has a session to wait for; a merely discovered
// one has none by definition, so it just reads online.
export function statusBadge(o: OutputInfo): { cls: string; text: string; title: string } {
  if (!o.present) return { cls: 'badge off', text: 'offline', title: 'Not on the network right now. Its routing is kept and reapplied when it returns.' };
  if (o.kind === 'pwsink' && o.state === 'adopted' && o.pwsink_streaming !== true) {
    return {
      cls: 'badge caution',
      text: 'not connected',
      title:
        "On the network, but no receiver has connected to the session we advertise — its module-rtp-session initiates the handshake, so until it does, anything routed here isn't played. Announcements open a temporary session instead.",
    };
  }
  return { cls: 'badge on', text: 'online', title: 'On the network and carrying audio routed to it.' };
}

// PTP-lock badge for an AirPlay-2 output — tri-state, because a lock is not
// needed for single-room realtime playback (the receiver free-runs off our
// PT=87 anchors); it only prevents drift in a multi-room group. So we only
// *alarm* (red) when the receiver is both unlocked AND in a ≥2-member group.
// Returns null when no badge should show (non-AP2 or offline).
export function ptpBadge(o: OutputInfo): { cls: string; text: string; title: string } | null {
  if (o.kind !== 'airplay2' || !o.present) return null;
  const age = o.ptp_lock_age_s != null ? ` (last clock sync ${o.ptp_lock_age_s}s ago)` : '';
  if (o.ptp_locked) {
    return { cls: 'badge on', text: 'PTP ✓', title: `Exchanging PTP with our clock${age} — multi-room sync is tight.` };
  }
  if (o.ptp_supported === false) {
    return {
      cls: 'badge',
      text: 'PTP n/a',
      title: "This receiver doesn't advertise PTP support (features bit 41); our sender streams realtime without it.",
    };
  }
  if (o.ptp_relevant) {
    return {
      cls: 'badge warn',
      text: 'no PTP lock',
      title: `Not exchanging PTP with us${age}. This receiver is in a multi-room AirPlay-2 group, so without a shared clock the rooms can drift out of sync. Re-route it (disconnect then reconnect) to re-establish PTP.`,
    };
  }
  return {
    cls: 'badge',
    text: 'PTP —',
    title: `Not exchanging PTP with us${age} — fine for single-room realtime playback; a live PTP lock only matters for keeping multiple rooms in sync.`,
  };
}

// Which outputs can be announced to individually via the per-device path
// (/api/announce). Every output kind is a per-device sender wired into the
// OverlayMixer: sendspin, AirPlay 2 (own overlay path), and pw-sink (the
// per-target AppleMIDI relay applies mix_into). Neither dialed backend needs a
// wired input — the daemon opens an on-demand session for an unrouted AirPlay-2
// receiver or pw-sink target — so the gate is reachability (`present`), NOT
// whether a session is attached: standing one up is exactly what this does.
//
// For AirPlay 2 and pw-sink this deliberately works on a merely *discovered*
// device too: playing a tone is how you find out which speaker
// `ap2-dev-living-2` is before adding it.
//
// Sendspin is the exception. A sendspin speaker is only reachable while the
// daemon holds a WebSocket to it, and it doesn't open one to a device that
// hasn't been added — and there's no on-demand equivalent, because a fresh
// sendspin connection takes tens of seconds to start rendering (ESPHome
// firmware), so a "test tone" over it would land long after you'd stopped
// listening. Add it first; then it's instantly testable like anything else.
export function canTest(o: OutputInfo): boolean {
  if (!o.present) return false;
  if (o.kind === 'sendspin') return o.state === 'adopted';
  return o.kind === 'airplay2' || o.kind === 'pwsink';
}
export function testHint(o: OutputInfo): string {
  if (!o.present) return 'Output is offline';
  if (o.kind === 'sendspin' && o.state !== 'adopted')
    return 'Add this speaker first — the router only connects to a Sendspin device once it has been added, and a fresh connection takes tens of seconds to start playing, so there is no instant test tone before that';
  // No session attached yet (`pwsink_streaming`), so the clip rides an on-demand
  // one — not `isOnline`, which is only reachability.
  if (o.kind === 'pwsink' && o.pwsink_streaming !== true)
    return 'Opens a temporary session — the target connects to it, so audio starts a moment later';
  return '';
}
