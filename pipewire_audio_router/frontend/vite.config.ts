import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `base: './'` emits relative asset URLs, so the built page works both served
// directly at :8099 and proxied under Home Assistant ingress (which mounts the
// add-on under a dynamic `/api/hassio_ingress/<token>/` path). All API/WS calls
// are likewise resolved relative to `document.baseURI` (see src/lib/api.ts).
export default defineConfig({
  plugins: [svelte()],
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
