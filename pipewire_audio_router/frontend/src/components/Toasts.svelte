<script lang="ts">
  import { toasts, dismiss } from '../lib/toast';
</script>

<div class="toasts">
  {#each $toasts as t (t.id)}
    <button class="toast {t.kind}" onclick={() => dismiss(t.id)}>{t.text}</button>
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
    text-align: left;
    border: none;
    padding: 10px 14px;
    border-radius: 8px;
    color: #fff;
    font-size: 0.85rem;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.25);
    cursor: pointer;
  }
  .toast.error { background: var(--error-color); }
  .toast.success { background: var(--success-color); }
  .toast.info { background: var(--primary-color); }
</style>
