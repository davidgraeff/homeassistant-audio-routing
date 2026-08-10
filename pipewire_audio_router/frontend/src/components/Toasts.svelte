<script lang="ts">
  import { toasts, dismiss, type Toast } from '../lib/toast';

  /** Running the action answers the toast, so it goes away with the click. */
  function act(t: Toast) {
    t.action?.run();
    dismiss(t.id);
  }
</script>

<!-- Polite live region: an Undo offer is only useful if it's announced, and these
     never interrupt. -->
<div class="toasts" aria-live="polite">
  {#each $toasts as t (t.id)}
    <div class="toast {t.kind}">
      <!-- The text is still the dismiss target, as it has always been — the toast
           was one big button until it had to hold a second control. -->
      <button class="text" type="button" title="Dismiss" onclick={() => dismiss(t.id)}>{t.text}</button>
      {#if t.action}
        <button class="act" type="button" onclick={() => act(t)}>{t.action.label}</button>
      {/if}
    </div>
  {/each}
</div>

<style>
  .toasts {
    position: fixed;
    bottom: 16px;
    right: 16px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 100;
    max-width: min(90vw, 380px);
  }
  .toast {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px 6px 2px 8px;
    border-radius: 8px;
    color: #fff;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
  }
  .toast button {
    background: none;
    border: none;
    color: inherit;
    font-size: 0.85rem;
    border-radius: 6px;
  }
  .toast button:hover {
    box-shadow: none;
  }
  .text {
    flex: 1;
    text-align: left;
    padding: 8px 6px;
    cursor: pointer;
  }
  /* The action is the one thing here worth aiming at, so it looks like a button
     on the colored field rather than more text. */
  .act {
    flex: none;
    padding: 6px 10px;
    font-weight: 500;
    border: 1px solid rgba(255, 255, 255, 0.55);
    white-space: nowrap;
  }
  .act:hover {
    background: rgba(255, 255, 255, 0.18);
  }
  .toast.error { background: var(--error-color); }
  .toast.success { background: var(--success-color); }
  .toast.info { background: var(--primary-color); }
</style>
