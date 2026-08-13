// What is hosting this page — a Home Assistant panel, or a browser tab.
//
// Under **ingress** the add-on is an iframe inside the Home Assistant app, served from
// HA's own origin at `/api/hassio_ingress/<token>/`. Two things follow, and both are
// about not making the user say the same thing twice:
//
//   * The frame is **same-origin**, so the parent document is readable — including the
//     theme the user actually chose in Home Assistant. The panel should be light when
//     their HA is light and dark when it is dark, without a setting of its own.
//   * Our own theme control is therefore noise in that context, and is hidden.
//
// Served directly (the add-on's port, a browser tab) nothing above us has an opinion, so
// the app keeps its own auto/light/dark choice.
//
// Everything here degrades to "no host": if ingress ever stops being same-origin, reading
// the parent throws, we report nothing, and the app falls back to its own preference. It
// must never be a broken page — this is a colour scheme.

/** Are we inside a Home Assistant ingress panel?
 *
 *  The path prefix is HA's, generated per session (see `lib/route.svelte.ts` on why we
 *  never build URLs against it) — but recognising it is exactly what it is good for.
 *  `window.parent !== window` alone would also match a plain iframe embed, which is not
 *  something we want to inherit a theme from. */
export const inIngress =
  window.parent !== window && location.pathname.includes('/api/hassio_ingress/');

export type HostTheme = 'light' | 'dark';

/** The parent's `<html>`, or null when it cannot be read (cross-origin, no parent). */
function hostRoot(): HTMLElement | null {
  if (!inIngress) return null;
  try {
    return window.parent.document.documentElement;
  } catch {
    return null; // cross-origin: nothing to follow, and not an error worth showing
  }
}

/** Relative luminance of a CSS colour, or null if it isn't one we can read.
 *
 *  Resolved through a throwaway element rather than parsed: the browser turns any colour
 *  notation — `#111`, `rgb()`, `color-mix()`, a named colour — into `rgb()` for us, and
 *  Home Assistant themes are written by hand in all of them. */
function luminance(color: string): number | null {
  const probe = document.createElement('span');
  probe.style.display = 'none';
  probe.style.color = color;
  // `documentElement` as the fallback parent: this runs while the theme is being decided,
  // which is the earliest thing the app does, and an exception here would be a white page
  // rather than a wrong colour.
  (document.body ?? document.documentElement).appendChild(probe);
  const resolved = getComputedStyle(probe).color;
  probe.remove();
  const m = /^rgba?\(([^)]+)\)$/.exec(resolved);
  if (!m) return null;
  const [r, g, b] = m[1].split(/[\s,/]+/).map(Number);
  if ([r, g, b].some((v) => !Number.isFinite(v))) return null;
  // Rec. 709, on 0–1. Good enough to answer "is this a dark surface"; we are not
  // grading contrast here.
  return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
}

/** The light/dark scheme of the Home Assistant app hosting us, or null if there is none
 *  to read.
 *
 *  Read off the *background* the parent is painting rather than a theme name: HA has any
 *  number of themes, community ones included, and only their own `dark` flag says which
 *  are dark — while `--primary-background-color` is set by every one of them and is the
 *  surface our own cards will sit against. `color-scheme` is the fallback for a theme
 *  that somehow sets one and not the other. */
export function hostTheme(): HostTheme | null {
  const root = hostRoot();
  if (!root) return null;
  const style = getComputedStyle(root);
  for (const prop of ['--primary-background-color', '--card-background-color']) {
    const lum = luminance(style.getPropertyValue(prop).trim());
    if (lum !== null) return lum < 0.5 ? 'dark' : 'light';
  }
  const scheme = style.colorScheme;
  if (scheme.includes('dark') && !scheme.includes('light')) return 'dark';
  if (scheme.includes('light')) return 'light';
  return null;
}

/** Call `onChange` whenever the host's theme changes, and return an unsubscribe.
 *
 *  Home Assistant applies a theme by writing custom properties onto its own `<html>`, so
 *  an attribute observer on that element is the notification — there is no event for it,
 *  and polling a colour four times a second to catch something that changes twice a day
 *  would be absurd. A no-op when there is no readable host. */
export function onHostThemeChange(onChange: (t: HostTheme) => void): () => void {
  const root = hostRoot();
  if (!root) return () => {};
  let last = hostTheme();
  const observer = new MutationObserver(() => {
    const next = hostTheme();
    if (next === null || next === last) return;
    last = next;
    onChange(next);
  });
  observer.observe(root, { attributes: true, attributeFilter: ['style', 'class'] });
  return () => observer.disconnect();
}
