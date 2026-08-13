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
    sources: 'Sources',
    outputs: 'Outputs',
    music: 'Music groups',
    announcements: 'Announcements',
    alignment: 'Alignment',
    settings: 'Settings',
    diagnostics: 'Diagnostics',
  };

  /** The pages that have to be visited *in this order* before anything plays — drawn as a
   *  chevron flow rather than as three tabs among seven (see `PAGES`). Taken as the first
   *  N of `PAGES` instead of listed again, so the bar cannot disagree with the route order
   *  about which pages are the setup path. */
  const SETUP_STEPS = 3;
  const setup = $derived(PAGES.slice(0, SETUP_STEPS));
  const rest = $derived(PAGES.slice(SETUP_STEPS));
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
  <!-- The setup path, as a chevron flow: the shape carries "in this order, and all three
       before anything plays", which three tabs among seven cannot say. The numbers match
       the wizard's stepper convention (AlignWizard's `.steps`) so the app has one visual
       language for "step N of a sequence". -->
  <span class="flow">
    {#each setup as id, i (id)}
      <a
        href={route.href(id)}
        class="step"
        class:active={tab === id}
        aria-current={tab === id ? 'page' : undefined}
      >
        <!-- Decorative: the label already names the page, and a screen reader announcing
             "1 Sources" adds nothing a sighted user gets from the arrow. -->
        <span class="n" aria-hidden="true">{i + 1}</span>{LABELS[id]}
      </a>
    {/each}
  </span>
  {#each rest as id (id)}
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
    align-items: stretch;
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

  /* --- the setup flow -------------------------------------------------- */

  .flow {
    display: flex;
    align-items: stretch;
    /* Reads as one object, then a gap before the ordinary tabs — without a rule, which
       would compete with the active tab's underline for the eye. */
    margin-right: 12px;
  }
  .flow .step {
    position: relative;
    /* So the chevron background (::before, z-index -1) cannot fall behind the bar. */
    isolation: isolate;
    display: inline-flex;
    align-items: center;
    gap: 7px;
    /* Asymmetric: the left notch eats into the padding, so the label still looks centred. */
    padding: 12px 18px 12px 26px;
    /* The fill is the "you are here", so no underline to fight with it. */
    border-bottom: none;
  }
  /* The shape lives on a pseudo-element rather than on the anchor, so `clip-path` cannot
     clip away the keyboard focus ring — which it does, invisibly, when applied directly. */
  .flow .step::before {
    content: '';
    position: absolute;
    inset: 0;
    z-index: -1;
    /* Mixed against the bar's own background rather than taken from a token: in the dark
       theme `--secondary-background-color` and `--card-background-color` are the *same*
       #1c1c1c, so the obvious choice made the chevrons invisible there. A percentage of the
       text colour is a contrast that holds in both themes by construction. */
    background: color-mix(in srgb, var(--primary-text-color) 9%, var(--card-background-color));
    clip-path: polygon(0 0, calc(100% - 11px) 0, 100% 50%, calc(100% - 11px) 100%, 0 100%, 11px 50%);
  }
  /* First step: flat left edge, nothing points into it. */
  .flow .step:first-child {
    padding-left: 16px;
  }
  .flow .step:first-child::before {
    clip-path: polygon(0 0, calc(100% - 11px) 0, 100% 50%, calc(100% - 11px) 100%, 0 100%);
  }
  /* 2px of the bar showing through is the separator; a border cannot follow the notch. */
  .flow .step + .step {
    margin-left: 2px;
  }
  .flow .step:hover::before {
    background: color-mix(in srgb, var(--primary-color) 22%, var(--card-background-color));
  }
  .flow .step.active {
    color: var(--text-on-primary);
    font-weight: 500;
  }
  .flow .step.active::before,
  .flow .step.active:hover::before {
    background: var(--primary-color);
  }
  /* The numbered circle, matching the alignment wizard's stepper. */
  .flow .n {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 17px;
    height: 17px;
    border-radius: 50%;
    font-size: 0.68rem;
    /* One step darker than the chevron it sits on, by the same construction. */
    background: color-mix(in srgb, var(--primary-text-color) 20%, var(--card-background-color));
    color: var(--primary-text-color);
  }
  .flow .step.active .n {
    background: var(--text-on-primary);
    color: var(--primary-color);
  }

  /* Narrow (a phone in the Home Assistant app): the bar already scrolls, but the chevrons
     are the widest thing in it, so they give up their generous padding first. */
  @media (max-width: 640px) {
    .flow .step {
      padding: 12px 12px 12px 20px;
      gap: 5px;
    }
    .flow .step:first-child {
      padding-left: 12px;
    }
  }

  main {
    max-width: 960px;
    margin: 0 auto;
    padding: 20px;
  }
</style>
