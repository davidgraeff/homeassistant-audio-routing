// Light/dark theme selection. `auto` follows the OS preference; `light`/`dark`
// force it via a `data-theme` attribute the CSS keys off (see app.css).

import { writable } from 'svelte/store';

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
apply(saved);

export const theme = writable<Theme>(saved);
theme.subscribe((t) => {
  localStorage.setItem(KEY, t);
  apply(t);
});
