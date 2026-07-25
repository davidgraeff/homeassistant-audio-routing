<script lang="ts">
  import { routing } from './lib/routing';
  import ThemeToggle from './components/ThemeToggle.svelte';
  import Toasts from './components/Toasts.svelte';
  import OutputsTab from './components/OutputsTab.svelte';
  import GroupsTab from './components/GroupsTab.svelte';
  import AlignTab from './components/AlignTab.svelte';
  import SourcesTab from './components/SourcesTab.svelte';
  import SettingsTab from './components/SettingsTab.svelte';
  import DiagnosticsTab from './components/DiagnosticsTab.svelte';

  // Groups is the primary surface. The low-level routing matrix moved into
  // Diagnostics (a connection-diagnosis tool, not a daily-use page).
  type Tab = 'groups' | 'outputs' | 'sources' | 'align' | 'settings' | 'diagnostics';
  let tab = $state<Tab>('groups');
  const tabs: { id: Tab; label: string }[] = [
    { id: 'groups', label: 'Groups' },
    { id: 'outputs', label: 'Outputs' },
    { id: 'sources', label: 'Sources' },
    { id: 'align', label: 'Align' },
    { id: 'settings', label: 'Settings' },
    { id: 'diagnostics', label: 'Diagnostics' },
  ];
</script>

<header class="app-header">
  <div class="brand">
    <span class="dot" class:on={$routing.connected} title={$routing.connected ? 'Connected' : 'Disconnected'}></span>
    <h1>PipeWire Audio Router</h1>
  </div>
  <ThemeToggle />
</header>

<nav class="tabs" aria-label="Sections">
  {#each tabs as t (t.id)}
    <button class:active={tab === t.id} onclick={() => (tab = t.id)}>{t.label}</button>
  {/each}
</nav>

<main>
  {#if tab === 'groups'}
    <GroupsTab />
  {:else if tab === 'outputs'}
    <OutputsTab />
  {:else if tab === 'sources'}
    <SourcesTab />
  {:else if tab === 'align'}
    <AlignTab />
  {:else if tab === 'settings'}
    <SettingsTab />
  {:else}
    <DiagnosticsTab />
  {/if}
</main>

<Toasts />

<style>
  .app-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 12px 20px;
    background: var(--app-header-background-color);
    color: var(--app-header-text-color);
    box-shadow: 0 2px 6px rgba(0, 0, 0, 0.15);
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .app-header h1 {
    margin: 0;
    font-size: 1.15rem;
    font-weight: 500;
  }
  .dot {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--disabled-text-color);
    box-shadow: 0 0 0 2px rgba(255, 255, 255, 0.25);
  }
  .dot.on {
    background: var(--success-color);
  }
  /* Header uses a colored background in both themes, so force the toggle button
     to read on it. */
  .app-header :global(button.ghost) {
    color: var(--app-header-text-color);
    border-color: rgba(255, 255, 255, 0.4);
  }

  .tabs {
    display: flex;
    gap: 4px;
    padding: 0 20px;
    background: var(--card-background-color);
    border-bottom: 1px solid var(--divider-color);
    overflow-x: auto;
  }
  .tabs button {
    background: transparent;
    border: none;
    border-radius: 0;
    padding: 12px 16px;
    color: var(--secondary-text-color);
    border-bottom: 2px solid transparent;
  }
  .tabs button:hover {
    box-shadow: none;
    color: var(--primary-text-color);
  }
  .tabs button.active {
    color: var(--primary-color);
    border-bottom-color: var(--primary-color);
    font-weight: 500;
  }

  main {
    max-width: 960px;
    margin: 0 auto;
    padding: 20px;
  }
</style>
