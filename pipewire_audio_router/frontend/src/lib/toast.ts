// Tiny transient-notification store used for API success/error feedback.

import { writable } from 'svelte/store';

export type ToastKind = 'error' | 'success' | 'info';

/** A one-click affordance carried by a toast — an Undo, in practice. */
export interface ToastAction {
  label: string;
  run: () => void;
}

export interface Toast {
  id: number;
  kind: ToastKind;
  text: string;
  action?: ToastAction;
}

export interface ToastOptions {
  action?: ToastAction;
  /** Lifetime in ms, overriding the per-kind default. */
  ttl?: number;
}

/** An Undo has to outlive reading the sentence that offers it, so a toast
 *  carrying an action stays up far longer than a bare confirmation does. */
const ACTION_TTL = 10_000;

let seq = 0;
export const toasts = writable<Toast[]>([]);

export function toast(kind: ToastKind, text: string, opts: ToastOptions = {}): void {
  const id = ++seq;
  toasts.update((list) => [...list, { id, kind, text, action: opts.action }]);
  const ttl = opts.ttl ?? (opts.action ? ACTION_TTL : kind === 'error' ? 6000 : 3000);
  setTimeout(() => toasts.update((list) => list.filter((t) => t.id !== id)), ttl);
}

export function dismiss(id: number): void {
  toasts.update((list) => list.filter((t) => t.id !== id));
}

/** Runs an async API call, surfacing success/error as toasts. Returns whether it succeeded. */
export async function run(action: () => Promise<{ ok?: boolean; message?: string } | unknown>, okText?: string): Promise<boolean> {
  try {
    const res = (await action()) as { ok?: boolean; message?: string } | null;
    if (res && typeof res === 'object' && 'ok' in res && res.ok === false) {
      toast('error', res.message ?? 'Request failed');
      return false;
    }
    if (okText) toast('success', okText);
    return true;
  } catch (e) {
    toast('error', e instanceof Error ? e.message : String(e));
    return false;
  }
}

/** `run`, for something the daemon can be told to take back: on success the toast
 *  offers **Undo** rather than only reporting what happened. That is what replaces
 *  asking first for anything reversible — the cost of the wrong click is one more
 *  click, which is cheaper than a dialog on every right one.
 *
 *  `undo` is itself put through `run`, so it should *throw* on a rejected response
 *  (that is what surfaces the reason), and is the place to refresh whatever page
 *  state doesn't come over the WebSocket. */
export async function runUndoable(
  action: () => Promise<{ ok?: boolean; message?: string } | unknown>,
  okText: string,
  undo: () => Promise<unknown>,
  undoneText: string,
): Promise<boolean> {
  if (!(await run(action))) return false;
  toast('success', okText, { action: { label: 'Undo', run: () => void run(undo, undoneText) } });
  return true;
}
