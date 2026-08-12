/**
 * Where a volume / mute change for one output has to be sent.
 *
 * Every output kind carries its level over its own transport — sendspin in-band over
 * its protocol, AirPlay 2 as an RTSP `SET_PARAMETER`, a pw-sink host through its
 * receiver agent — so there is one endpoint per kind and they do not even agree on a
 * scale (sendspin is 0–100, the other two are 0.0–1.0). The routing graph knows only a
 * node name, the Outputs page knows the whole `OutputInfo`, and both used to pick the
 * endpoint themselves with `kind === 'airplay2' ? ap2 : sendspin`.
 *
 * That fallback is what made pw-sink volume and mute silently do nothing: a
 * `pwsink-dev-*` name went to `PUT /api/sendspin/volume`, which happily *stored* it as
 * an intent for a sendspin device that will never connect and answered `ok: true`. The
 * click therefore looked accepted, and a moment later the pushed matrix frame — which
 * carries what the *host* reports — put the old value back. That is exactly the
 * "mute flips back on its own" symptom.
 *
 * So the mapping lives here, once, keyed off the node-name prefix the daemon assigns
 * (`routing::PWSINK_DEV_PREFIX` and friends). A new output kind adds a branch here and
 * both call sites get it; forgetting to add one is a loud 404-shaped mistake in one
 * place instead of a wrong-but-successful write in two.
 */
import { api } from '../api';
import type { OpResponse } from '../types';

export const AP2_DEV_PREFIX = 'ap2-dev-';
export const SENDSPIN_DEV_PREFIX = 'sendspin-dev-';
export const PWSINK_DEV_PREFIX = 'pwsink-dev-';

/** Which endpoint pair drives this output's level. */
export type LevelBackend = 'airplay2' | 'sendspin' | 'pwsink';

export function levelBackend(nodeName: string): LevelBackend {
  if (nodeName.startsWith(AP2_DEV_PREFIX)) return 'airplay2';
  if (nodeName.startsWith(PWSINK_DEV_PREFIX)) return 'pwsink';
  return 'sendspin';
}

/** Set one output's volume. `pct` is 0–100 — what every slider in the UI works in —
 *  and is converted to each backend's own scale here. */
export function setOutputVolume(nodeName: string, pct: number): Promise<OpResponse> {
  switch (levelBackend(nodeName)) {
    case 'airplay2':
      return api.setAp2Volume(nodeName, pct / 100);
    case 'pwsink':
      return api.setPwsinkVolume(nodeName, pct / 100);
    case 'sendspin':
      return api.setSendspinVolume(nodeName, pct);
  }
}

export function setOutputMute(nodeName: string, muted: boolean): Promise<OpResponse> {
  switch (levelBackend(nodeName)) {
    case 'airplay2':
      return api.setAp2Mute(nodeName, muted);
    case 'pwsink':
      return api.setPwsinkMute(nodeName, muted);
    case 'sendspin':
      return api.setSendspinMute(nodeName, muted);
  }
}
