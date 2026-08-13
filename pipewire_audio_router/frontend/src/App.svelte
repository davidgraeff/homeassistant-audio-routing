<script lang="ts">
  import { routing } from './lib/routing';
  import { HOME, PAGES, route, type Page } from './lib/route.svelte';
  import { themeIsOurs } from './lib/theme';
  import ThemeToggle from './components/ui/ThemeToggle.svelte';
  import HomePage from './components/home/HomePage.svelte';
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

  // How the bar gives way as it narrows, in Home Assistant's own order of sacrifice: the
  // labels of the later sections go first (icons only), then those sections collapse into a
  // ⋮ menu, and only then does the app's own name give up everything but its mark. The
  // setup chevrons never leave — they are the one thing a half-set-up add-on must show.
  // Purely CSS breakpoints (see the media queries below); this state is only the menu.
  let menuOpen = $state(false);

  /** A pointer anywhere outside the menu closes it — the ordinary dismiss for a popup that
   *  has no backdrop of its own. */
  function onGlobalDown(e: PointerEvent) {
    if (!menuOpen) return;
    if ((e.target as Element | null)?.closest('.menuwrap')) return;
    menuOpen = false;
  }
</script>

<svelte:window
  onpointerdown={onGlobalDown}
  onkeydown={(e) => {
    if (e.key === 'Escape') menuOpen = false;
  }}
/>

<!-- The later sections, as glyphs. Same 16-box stroke-on-currentColor idiom as the rest of
     the app's icons, so they inherit the tab's own colour and its active state. -->
{#snippet icon(id: Page)}
  <svg class="ico" viewBox="0 0 16 16" aria-hidden="true">
    {#if id === 'announcements'}
      <!-- A speech bubble: this is the page about talking over the music. -->
      <path d="M2.2 3.4h11.6v7.4H6.6L3.6 13.2v-2.4H2.2z" />
    {:else if id === 'alignment'}
      <!-- A measurement between two points — what an alignment run does. -->
      <path d="M2.6 4.2v7.6M13.4 4.2v7.6M2.6 8h10.8M8 6.2v3.6" />
    {:else if id === 'settings'}
      <!-- The same sliders the announcement group's own settings button uses. -->
      <path d="M2 5h6.4M11.6 5H14M2 11h2.4M7.6 11H14" />
      <circle cx="10" cy="5" r="1.6" />
      <circle cx="6" cy="11" r="1.6" />
    {:else}
      <!-- Diagnostics: a trace, because that is what the page is full of. -->
      <path d="M1.8 8.6h2.9l2-4.4 2.2 7.6 1.8-3.2h3.5" />
    {/if}
  </svg>
{/snippet}

<!-- One bar, not two.
     Under ingress Home Assistant is already drawing a header above us — its own title on a
     phone, the sidebar's on a desktop — so a second full-width title bar of our own was a
     duplicate on every screen and a *doubled* one on a narrow screen. The title, the
     sections and the connection state now share the row that HA's own header aligns to:
     same height, same surface, so the two read as one chrome rather than two.

     Anchors, not buttons: these *are* links now that a page has a URL, so middle-click,
     "open in new tab" and the browser's own link affordances work without a handler. The
     fragment change is the navigation — nothing here writes the page state. -->
<header class="app-bar">
  <!-- The title is the way to the front page, as it is in every other app. -->
  <a class="brand" href={route.href(HOME)} class:active={tab === HOME} aria-current={tab === HOME ? 'page' : undefined} title="What this add-on is, and how to set it up">
    <!-- The mark carries the connection state in its own ring: green while the socket is up,
         with one arc travelling slowly round it, and a plain red ring when it is not. That
         used to be a separate dot beside the title, which needed its own space in the bar
         and read as punctuation in the name. -->
    <span class="markwrap">
      <img class="mark" src="./favicon.svg" alt="" width="22" height="22" />
      <svg
        class="ring"
        class:on={$routing.connected}
        viewBox="0 0 32 32"
        role="img"
        aria-label={$routing.connected ? 'Connected to the add-on' : 'Not connected — the add-on may be restarting'}
      >
        <rect class="track" x="1.25" y="1.25" width="29.5" height="29.5" rx="9" pathLength="100" />
        <!-- `pathLength` normalises the perimeter to 100, so the dash below is a fixed
             fraction of the ring whatever the geometry is. -->
        <rect class="chase" x="1.25" y="1.25" width="29.5" height="29.5" rx="9" pathLength="100" />
      </svg>
    </span>
    <span class="name">PipeWire Audio Router</span>
  </a>

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
    <!-- `title` on every one of these, not only in icon mode: the label is gone at some
         widths and there either way for a screen reader. -->
    {#each rest as id (id)}
      <a class="rest" href={route.href(id)} title={LABELS[id]} class:active={tab === id} aria-current={tab === id ? 'page' : undefined}>
        {@render icon(id)}
        <span class="lbl">{LABELS[id]}</span>
      </a>
    {/each}
  </nav>

  <div class="state">
    <!-- The same sections again, behind one button, for when even their icons don't fit.
         Only one of the two is ever visible (media queries), so this is not a duplicate
         control — it is the same nav at a smaller size. -->
    <div class="menuwrap">
      <button
        class="menubtn"
        type="button"
        aria-haspopup="true"
        aria-expanded={menuOpen}
        aria-label="More sections"
        title="More sections"
        onclick={() => (menuOpen = !menuOpen)}
      >
        <svg class="ico dots" viewBox="0 0 16 16" aria-hidden="true">
          <circle cx="8" cy="3.2" r="1.4" />
          <circle cx="8" cy="8" r="1.4" />
          <circle cx="8" cy="12.8" r="1.4" />
        </svg>
      </button>
      {#if menuOpen}
        <div class="menu" role="menu">
          {#each rest as id (id)}
            <a
              href={route.href(id)}
              role="menuitem"
              class:active={tab === id}
              aria-current={tab === id ? 'page' : undefined}
              onclick={() => (menuOpen = false)}
            >
              {@render icon(id)}
              {LABELS[id]}
            </a>
          {/each}
        </div>
      {/if}
    </div>
    <!-- Hidden inside a Home Assistant panel: the theme there is whatever HA is using,
         followed live (lib/theme.ts), so there is nothing to choose. -->
    {#if themeIsOurs}
      <ThemeToggle />
    {/if}
  </div>
</header>

<main>
  {#if tab === HOME}
    <HomePage />
  {:else if tab === 'music'}
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
  /* 56px is Home Assistant's own header height (`--header-height`), which is what the
     sidebar's "Home Assistant" row and a panel's title row are both drawn at. Matching it
     — and the card surface, rather than the accent colour the bar used to be painted in —
     is what makes our title sit on the same line as HA's instead of just below it in a
     different colour. */
  .app-bar {
    display: flex;
    align-items: stretch;
    min-height: 56px;
    padding: 0 8px 0 16px;
    background: var(--card-background-color);
    border-bottom: 1px solid var(--divider-color);
  }
  /* The title, and the link to the front page. Never scrolls away with the sections: it is
     both the app's name and the way back, so it keeps its place. */
  .brand {
    display: flex;
    flex: none;
    align-items: center;
    gap: 9px;
    padding-right: 16px;
    color: var(--primary-text-color);
    text-decoration: none;
    font-size: 1.25rem; /* HA's own header title size */
    font-weight: 400;
    white-space: nowrap;
  }
  .brand:hover .name {
    color: var(--primary-color);
  }
  .brand.active .name {
    font-weight: 500;
  }
  /* The mark, with the connection ring around it. */
  .markwrap {
    position: relative;
    flex: none;
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
  }
  .mark {
    border-radius: 5px;
  }
  .ring {
    position: absolute;
    inset: 0;
    width: 32px;
    height: 32px;
    fill: none;
    stroke-width: 2;
    stroke-linejoin: round;
  }
  /* Not connected: a solid red ring and nothing moving, which is the honest picture — the
     travelling arc means "there is a live socket", so it must not run when there isn't. */
  .ring .track {
    stroke: var(--error-color);
  }
  .ring .chase {
    display: none;
  }
  .ring.on .track {
    stroke: color-mix(in srgb, var(--success-color) 30%, transparent);
  }
  .ring.on .chase {
    display: block;
    stroke: var(--success-color);
    stroke-linecap: round;
    /* `pathLength="100"` above makes these percentages of the ring: a fifth of it, going
       round once every 7s — slow enough to read as a heartbeat rather than a spinner. */
    stroke-dasharray: 20 80;
    animation: chase 7s linear infinite;
  }
  @keyframes chase {
    to {
      stroke-dashoffset: -100;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .ring.on .chase {
      animation: none;
    }
  }
  .state {
    display: flex;
    flex: none;
    align-items: center;
    gap: 8px;
    padding-left: 8px;
  }

  .tabs {
    display: flex;
    flex: 1;
    align-items: stretch;
    gap: 4px;
    min-width: 0;
    /* The bar is meant to *fit* at every width — that is what the breakpoints at the bottom
       of this file are for — so there is no scrollbar to see. Scrolling stays possible only
       as a safety net: if a font renders wider than the breakpoints assume, a section is
       still reachable rather than clipped away. */
    overflow-x: auto;
    scrollbar-width: none;
  }
  .tabs::-webkit-scrollbar {
    display: none;
  }
  /* Icon + label; which of the two shows is the breakpoints' business. */
  .rest {
    gap: 8px;
  }
  .ico {
    flex: none;
    width: 17px;
    height: 17px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .ico circle {
    fill: currentColor;
    stroke: none;
  }
  .dots circle {
    stroke: none;
  }
  /* Labels showing, so the glyphs would only be decoration. */
  .rest .ico {
    display: none;
  }

  /* --- the overflow menu ------------------------------------------------ */

  .menuwrap {
    position: relative;
    display: none; /* only exists once the sections stop fitting */
  }
  .menubtn {
    display: inline-flex;
    align-items: center;
    padding: 7px;
    border: none;
    background: none;
    color: var(--secondary-text-color);
  }
  .menubtn:hover:not(:disabled) {
    box-shadow: none;
    color: var(--primary-text-color);
  }
  .menubtn[aria-expanded='true'] {
    color: var(--primary-color);
  }
  .menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 40;
    min-width: 190px;
    padding: 6px;
    border: 1px solid var(--ha-card-border-color, var(--divider-color));
    border-radius: 10px;
    background: var(--card-background-color);
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.25);
  }
  .menu a {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 9px 10px;
    border-radius: 7px;
    color: var(--primary-text-color);
    text-decoration: none;
    white-space: nowrap;
  }
  .menu a:hover {
    background: color-mix(in srgb, var(--primary-text-color) 7%, transparent);
  }
  .menu a.active {
    color: var(--primary-color);
    font-weight: 500;
  }
  .menu .ico {
    display: block;
  }
  /* Full-height hit targets in a 56px bar: the vertical padding is gone, so the label is
     centred by the flex box and the active underline sits on the bar's own bottom edge. */
  .tabs a {
    display: inline-flex;
    align-items: center;
    padding: 0 16px;
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
    padding: 0 18px 0 26px;
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

  /* --- how the bar gives way ---------------------------------------------
     Three steps, in Home Assistant's own order: labels → icons → a ⋮ menu → the app's name
     down to its mark. The setup chevrons never leave.

     The numbers are the measured widths of the parts, not guesses: brand with its name 283,
     the three chevrons 426 (396 with the compact padding below), the four section labels
     487, their icons 212, the ⋮ 31, the theme control 98. So full labels need ~1302 and
     icons ~1027 — hence 1360 and 1060, which is those figures plus enough slack that the
     last section does not sit against the theme control (and for a font that renders wider
     than this one). Measured with the theme control present; a Home Assistant panel does not
     have it and so has ~100px more room at every width, which can only make these too
     cautious, never too tight.

     Breakpoints rather than measuring the bar at runtime: every element here has a fixed
     size, and a measured layout that switches mode changes the very width it measured —
     the classic resize loop.

     Below ~600px the chevrons alone are wider than the bar, and there is nothing left to
     give: `.tabs` scrolls there (with no visible scrollbar), which on a phone is a swipe. */

  /* 1. The later sections keep their icons only — the labels are what the setup chevrons,
        which cannot be dropped, need the room for. */
  @media (max-width: 1359px) {
    .rest .lbl {
      display: none;
    }
    .rest .ico {
      display: block;
    }
    .rest {
      padding: 0 11px;
    }
  }

  /* 2. Even the icons don't fit: they become one ⋮ button at the end of the bar. */
  @media (max-width: 1059px) {
    .tabs .rest {
      display: none;
    }
    .menuwrap {
      display: block;
    }
  }

  /* 3. Only now the app's own name gives way to its mark, and the chevrons give up their
        generous padding. On a phone Home Assistant draws the panel's name in its own header
        directly above us, so spelling it out again costs a third of the bar to say what the
        line above already says. */
  @media (max-width: 839px) {
    .brand .name {
      display: none;
    }
    .brand {
      padding-right: 8px;
    }
    .app-bar {
      padding-left: 10px;
    }
    .flow .step {
      padding: 0 12px 0 20px;
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
