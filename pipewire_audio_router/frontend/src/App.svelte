<script lang="ts">
  import { routing } from './lib/routing';
  import { PAGES, route, type Page } from './lib/route.svelte';
  import ThemeToggle from './components/ui/ThemeToggle.svelte';
  import Toasts from './components/ui/Toasts.svelte';
  import ConfirmDialog from './components/ui/ConfirmDialog.svelte';
  import OutputsTab from './components/outputs/OutputsTab.svelte';
  import AlignmentTab from './components/align/AlignmentTab.svelte';
  import MusicGroupsTab from './components/groups/MusicGroupsTab.svelte';
  import AnnouncementsTab from './components/groups/AnnouncementsTab.svelte';
  import SourcesTab from './components/sources/SourcesTab.svelte';
  import SettingsTab from './components/system/SettingsTab.svelte';
  import DiagnosticsTab from './components/system/DiagnosticsTab.svelte';

  // Music groups is the primary surface: who plays together, and what they play
  // (with the low-level routing graph on the same page). Announcements is the
  // other half of the group model.
  //
  // **Alignment is a page, not a panel** (plan §12.1). It is about a *set of speakers* the
  // user picks — never a source group — so it sits next to Outputs; and it is five wizard
  // pages deep, ending in a report that is read rather than glanced at, so it gets the room
  // a page has. Outputs keeps the way in and the "an alignment is holding these right now"
  // state, because those are what someone needs while looking at their speakers.
  //
  // Which page is open lives in the URL fragment (`./lib/route.svelte`), so a reload — an
  // add-on restart, or ingress reloading the panel iframe — comes back to the page the user
  // was on rather than to the front page.
  const tab = $derived(route.page);
  /** The tab bar, in `PAGES` order so the URL ids and the bar cannot drift apart. */
  const LABELS: Record<Page, string> = {
    music: 'Music groups',
    announcements: 'Announcements',
    outputs: 'Outputs',
    alignment: 'Alignment',
    sources: 'Sources',
    settings: 'Settings',
    diagnostics: 'Diagnostics',
  };
</script>

<header class="app-header">
  <div class="brand">
    <span class="dot" class:on={$routing.connected} title={$routing.connected ? 'Connected' : 'Disconnected'}></span>
    <h1>PipeWire Audio Router</h1>
  </div>
  <ThemeToggle />
</header>

<!-- Anchors, not buttons: these *are* links now that a page has a URL, so middle-click,
     "open in new tab" and the browser's own link affordances work without a handler. The
     fragment change is the navigation — nothing here writes the page state. -->
<nav class="tabs" aria-label="Sections">
  {#each PAGES as id (id)}
    <a href={route.href(id)} class:active={tab === id} aria-current={tab === id ? 'page' : undefined}>
      {LABELS[id]}
    </a>
  {/each}
</nav>

<main>
  {#if tab === 'music'}
    <MusicGroupsTab />
  {:else if tab === 'announcements'}
    <AnnouncementsTab />
  {:else if tab === 'outputs'}
    <!-- The one cross-page navigation in the app, and it is one-way: Outputs offers to
         align, and this owns the tab, so the page that starts an alignment does not have to
         know it is a tab at all. -->
    <OutputsTab />
  {:else if tab === 'alignment'}
    <!-- No exit callback: it is a tab, so this bar is the way out. -->
    <AlignmentTab />
  {:else if tab === 'sources'}
    <SourcesTab />
  {:else if tab === 'settings'}
    <SettingsTab />
  {:else}
    <DiagnosticsTab />
  {/if}
</main>

<Toasts />
<!-- One instance for the whole app: every `askConfirm()` renders here. -->
<ConfirmDialog />

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
  .tabs a {
    padding: 12px 16px;
    color: var(--secondary-text-color);
    border-bottom: 2px solid transparent;
    text-decoration: none;
    white-space: nowrap;
    font-size: inherit;
    font-family: inherit;
  }
  .tabs a:hover {
    color: var(--primary-text-color);
  }
  .tabs a.active {
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
