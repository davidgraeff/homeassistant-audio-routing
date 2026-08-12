<script lang="ts">
  // The app's confirmation dialog: one instance, mounted in App.svelte, showing
  // whatever `askConfirm()` has open (see lib/confirm.svelte.ts for why this
  // exists rather than `window.confirm`).
  //
  // Built on <dialog>/showModal(), which brings the focus trap, Escape, top-layer
  // stacking and focus-return-to-the-invoking-control with it — the *Docs.svelte
  // modals hand-roll those. The top layer is per-document, so it behaves the same
  // served directly and under Home Assistant ingress.
  import { confirmDialog, settleConfirm } from '../../lib/confirm.svelte';

  const req = $derived(confirmDialog.pending);
  const paragraphs = $derived(req?.body == null ? [] : Array.isArray(req.body) ? req.body : [req.body]);

  let el = $state<HTMLDialogElement>();
  let goEl = $state<HTMLButtonElement>();
  let cancelEl = $state<HTMLButtonElement>();

  $effect(() => {
    if (!el) return;
    if (req && !el.open) {
      el.showModal();
      // A destructive question opens with Cancel focused: Enter on a dialog you
      // weren't expecting must not be the thing that removes the speaker.
      (req.danger ? cancelEl : goEl)?.focus();
    } else if (!req && el.open) {
      el.close();
    }
  });
</script>

<!-- `close` covers Escape and our own `close()` alike, and settling twice is a
     no-op — so the one fired by closing an already-answered question is free.
     The click handler is the backdrop: `.pane` carries the padding, so a click
     landing on the dialog element itself is outside the card. -->
<dialog
  class="card confirm"
  bind:this={el}
  onclose={() => settleConfirm(false)}
  onclick={(e) => e.target === el && settleConfirm(false)}
>
  {#if req}
    <div class="pane">
      <h2>{req.title}</h2>
      {#each paragraphs as p, i (i)}
        <p>{p}</p>
      {/each}
      <div class="row">
        <button class="ghost" type="button" bind:this={cancelEl} onclick={() => settleConfirm(false)}>
          {req.cancelLabel ?? 'Cancel'}
        </button>
        <button
          class={req.danger ? 'danger' : 'primary'}
          type="button"
          bind:this={goEl}
          onclick={() => settleConfirm(true)}
        >
          {req.confirmLabel ?? 'OK'}
        </button>
      </div>
    </div>
  {/if}
</dialog>

<style>
  /* Narrower than the document modals (.modal-card): this is a sentence and two
     buttons, not a page of prose. */
  dialog.confirm {
    width: min(440px, calc(100vw - 2rem));
    padding: 0;
    color: var(--primary-text-color);
    /* <dialog> is centred by its own margin:auto; the UA border is replaced by
       .card's. */
    margin: auto;
  }
  dialog.confirm::backdrop {
    background: rgba(0, 0, 0, 0.5);
  }
  .pane {
    padding: 18px 20px 16px;
  }
  h2 {
    margin: 0;
    font-size: 1.05rem;
    font-weight: 500;
  }
  p {
    margin: 10px 0 0;
    font-size: 0.85rem;
    line-height: 1.45;
    color: var(--secondary-text-color);
  }
  .row {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 18px;
  }
</style>
