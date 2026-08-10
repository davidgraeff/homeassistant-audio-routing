import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// Builds the Lovelace card — a *separate* artifact from the add-on's admin UI
// (vite.config.ts), even though both live in this project and share the toolchain.
//
// It has to be one self-contained ES module with no import map and no sibling
// assets: Home Assistant loads it with a bare `<script type="module">` from the
// path the integration serves it on, and it is committed into the integration
// (which HACS copies verbatim, running no build step) rather than shipped in the
// add-on image.
//
// `emitCss: false` compiles the component styles into that module so they are
// injected at runtime; an emitted .css file would simply never be fetched.
export default defineConfig({
  plugins: [svelte({ emitCss: false })],
  build: {
    lib: {
      entry: 'src/card/card.ts',
      formats: ['es'],
      fileName: () => 'pipewire-router-card.js',
    },
    // Straight into the integration, so the built card is what gets committed and
    // installed. Never `emptyOutDir` — that directory is inside the integration.
    outDir: '../../custom_components/pipewire_audio_router/www',
    emptyOutDir: false,
    // Home Assistant's frontend is ES2022-era; no need to down-level, and a
    // smaller module means a faster first dashboard paint.
    target: 'es2022',
  },
});
