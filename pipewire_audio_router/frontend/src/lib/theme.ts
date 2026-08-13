// Light/dark theme selection. `auto` follows the OS preference; `light`/`dark`
// force it via a `data-theme` attribute the CSS keys off (see app.css).
//
// **Inside a Home Assistant ingress panel there is no choice to make**: the user already
// picked a theme in Home Assistant, and a panel that disagrees with the app around it
// looks broken rather than configurable. So the host's scheme wins there, it is followed
// live, and the control is not offered (`themeIsOurs` gates it) — see `lib/host.ts`.

import { writable, type Readable } from 'svelte/store';
import { hostTheme, inIngress, onHostThemeChange } from './host';

export type Theme = 'auto' | 'light' | 'dark';

const KEY = 'par-theme';

function apply(t: Theme): void {
  const root = document.documentElement;
  if (t === 'auto') root.removeAttribute('data-theme');
  else root.setAttribute('data-theme', t);
}

const saved = ((): Theme => {
  const v = localStorage.getItem(KEY);
  return v === 'light' || v === 'dark' || v === 'auto' ? v : 'auto';
})();

/** Is the theme this app's own business — i.e. is there a control to show?
 *
 *  False only when we are in a panel whose host we can actually read: ingress that has
 *  gone cross-origin leaves us with our own preference, so the control stays. */
export const themeIsOurs = !(inIngress && hostTheme() !== null);

const initial: Theme = themeIsOurs ? saved : (hostTheme() as Theme);
apply(initial);

const store = writable<Theme>(initial);

if (themeIsOurs) {
  store.subscribe((t) => {
    localStorage.setItem(KEY, t);
    apply(t);
  });
} else {
  // Followed, not stored: the saved preference is left untouched so it is still there if
  // the same browser opens the add-on directly.
  store.subscribe(apply);
  onHostThemeChange((t) => store.set(t));
}

/** The active theme. Writable when it is ours (the toggle), read-only under ingress —
 *  where writing it would be overwritten by the host on its next change anyway. */
export const theme = store as Readable<Theme> & Pick<typeof store, 'set' | 'update'>;
