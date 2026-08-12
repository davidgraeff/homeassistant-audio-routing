// Which page the app is on, kept in the URL fragment (`#/outputs`).
//
// The tab used to be plain component state, so every reload started on Music groups —
// and reloads are not rare here: the add-on is restarted while its UI is open (a source
// added, a setting written), and under Home Assistant ingress the panel iframe is
// reloaded whenever the user comes back to it. Someone three pages into Diagnostics
// would land back on the front page and have to find their way again.
//
// The **fragment** rather than a path, for two reasons: it is never sent to the server, so
// the daemon's `ServeDir` keeps serving one `index.html` with no SPA-fallback route to
// add; and it survives ingress, whose path prefix (`/api/hassio_ingress/<token>/`) is
// generated per session and is not ours to build URLs against.
//
// Navigation *pushes* a history entry, so the browser's back button undoes a tab switch —
// which is what a hash in the URL leads a user to expect. Normalising an empty or unknown
// fragment replaces instead, so arriving at the app doesn't leave a dead entry behind.

/** The pages, in tab-bar order. The fragment is one of these ids. */
export const PAGES = [
  'music',
  'announcements',
  'outputs',
  'alignment',
  'sources',
  'settings',
  'diagnostics',
] as const;

export type Page = (typeof PAGES)[number];

/** Where an empty or unrecognised fragment lands: music groups is the primary surface. */
export const DEFAULT_PAGE: Page = 'music';

const isPage = (v: string): v is Page => (PAGES as readonly string[]).includes(v);

/** The page a fragment names, or null if it names nothing we have.
 *
 *  Both `#/outputs` and `#outputs` are accepted — the canonical form we write is the
 *  first, but a hand-typed or hand-edited URL should still work. A stale fragment from an
 *  older version (or a typo) is *not* an error state worth showing: it resolves to null
 *  and the caller falls back to the default page. */
function parse(hash: string): Page | null {
  const id = decodeURIComponent(hash.replace(/^#\/?/, ''));
  return isPage(id) ? id : null;
}

const fragment = (p: Page) => `#/${p}`;

const initial = parse(location.hash) ?? DEFAULT_PAGE;

// Write the canonical fragment for whatever we resolved to, so the URL a user copies or
// bookmarks names the page they are looking at even when they arrived with no fragment at
// all. `replaceState` rather than assigning `location.hash`: this is the entry itself
// being corrected, not a navigation.
if (location.hash !== fragment(initial)) {
  history.replaceState(null, '', fragment(initial));
}

let current = $state<Page>(initial);

// The back/forward buttons and a hand-edited URL both arrive here, which is why the state
// is only ever written from this listener: `go()` changes the fragment and lets the
// browser tell us, so there is exactly one path from "the URL says X" to "we render X".
window.addEventListener('hashchange', () => {
  current = parse(location.hash) ?? DEFAULT_PAGE;
});

export const route = {
  /** The page to render. */
  get page(): Page {
    return current;
  },

  /** Go to a page. A no-op when already there, so re-clicking the active tab doesn't
   *  stack history entries the back button then has to be pressed through. */
  go(p: Page): void {
    if (p === current) return;
    location.hash = fragment(p);
  },

  /** The `href` for a link to a page — for anchors, which get middle-click, "open in new
   *  tab" and a status-bar preview for free. */
  href: fragment,
};
