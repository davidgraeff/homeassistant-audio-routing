// Tiny transient-notification store used for API success/error feedback.

import { writable } from 'svelte/store';

export type ToastKind = 'error' | 'success' | 'info';

export interface Toast {
  id: number;
  kind: ToastKind;
  text: string;
}

let seq = 0;
export const toasts = writable<Toast[]>([]);

export function toast(kind: ToastKind, text: string): void {
  const id = ++seq;
  toasts.update((list) => [...list, { id, kind, text }]);
  const ttl = kind === 'error' ? 6000 : 3000;
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
