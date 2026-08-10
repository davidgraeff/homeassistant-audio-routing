// Promise-based confirmation, in the app's own chrome — the replacement for
// `window.confirm()`.
//
// `confirm()` is drawn by the browser *outside* this document: it ignores the
// theme (the toggle in the header doesn't reach it), renders a `\n\n`-separated
// explanation as one flat wall, labels its buttons OK/Cancel whatever is being
// decided, and blocks the event loop — which also means the headless screenshot
// harness can never capture a confirmation state. Under Home Assistant ingress it
// is additionally a dialog attributed to the whole page, not to this add-on.
//
// Call `askConfirm()` and await the answer; `<ConfirmDialog>` (mounted once, in
// App.svelte) renders whatever is open. Only asked for the destructive and
// unrecoverable — anything the daemon can be told to take back gets an Undo
// toast instead (`runUndoable` in ./toast).

export interface ConfirmRequest {
  /** The question, *as* a question — "Remove 'Kitchen'?". Not "Are you sure?". */
  title: string;
  /** What happens if they go ahead. One paragraph per string. */
  body?: string | string[];
  /** Names the verb: "Remove", "Unpair", "Forget". Never "OK". */
  confirmLabel?: string;
  cancelLabel?: string;
  /** Destructive: the go-ahead button reads as such, and focus starts on Cancel. */
  danger?: boolean;
}

type Pending = ConfirmRequest & { resolve: (ok: boolean) => void };

let pending = $state<Pending | null>(null);

/** The open question, for `<ConfirmDialog>` to render. */
export const confirmDialog = {
  get pending() {
    return pending;
  },
};

/** Ask, and resolve with the answer (`false` on cancel, Escape or backdrop).
 *
 *  One question is open at a time. The dialog is modal, so a second one can only
 *  come from code — it takes over, cancelling the first rather than stranding its
 *  promise. */
export function askConfirm(req: ConfirmRequest): Promise<boolean> {
  pending?.resolve(false);
  return new Promise((resolve) => {
    pending = { ...req, resolve };
  });
}

/** Answer the open question. Idempotent — settling an already-answered one is a
 *  no-op, which is what lets the dialog settle on close unconditionally. */
export function settleConfirm(ok: boolean): void {
  const p = pending;
  pending = null;
  p?.resolve(ok);
}

// ---- Shared wording -------------------------------------------------------

/** Un-adopting an output. Asked from the Outputs page *and* from the routing
 *  graph's ✕ — one call on one thing, so one wording; the two used to explain
 *  themselves differently. */
export function removeOutputConfirm(name: string, present: boolean): ConfirmRequest {
  return {
    title: `Remove '${name}' from your outputs?`,
    body: [
      'Its routing, group membership and Home Assistant media_player are removed.',
      present
        ? `'${name}' stays on the network, so it comes back under Discovered on the Outputs page.`
        : `'${name}' is offline, so it disappears until it shows up again — as a discovered device on the Outputs page.`,
    ],
    confirmLabel: 'Remove',
    danger: true,
  };
}
