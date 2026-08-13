# Branding

`icon.svg` and `logo.svg` are the masters. Everything else is generated:

```bash
./scripts/render-branding.sh
```

The generated PNGs are committed, because neither Supervisor nor
home-assistant/brands can consume SVG.

## The mark

One source fanning out to three independent outputs — the routing matrix in a
single glyph. Colours come straight from the add-on UI
(`pipewire_audio_router/frontend/src/app.css`): accent orange `#ff9800` for the
source, white signal paths, the dark navy surface of the app's dark theme. The
lockup carries its own surface so it stays legible on both the light and the
dark add-on store page.

It stays readable down to 24 px (sidebar size); verified by rasterising at 24 /
32 / 48 px.

## Where each file goes

| File | Consumer |
| --- | --- |
| `pipewire_audio_router/icon.png` (256×256) | Supervisor add-on store + sidebar |
| `pipewire_audio_router/logo.png` (500×200) | Supervisor add-on detail page header |
| `pipewire_audio_router/frontend/public/favicon.svg` | browser tab of the add-on web UI |
| `pipewire_audio_router/frontend/public/apple-touch-icon.png` (180×180) | iOS/Android home-screen shortcut to the UI |
| `custom_components/pipewire_audio_router/brand/*` | the integration, via HA's brands proxy — see below |

The add-on picks `icon.png`/`logo.png` up by convention — no `config.yaml` entry
needed. The add-on's *sidebar panel* icon stays the MDI glyph
(`panel_icon: mdi:speaker-multiple`), because HA only accepts an MDI name there.

## Custom integration icon: shipped in-tree, no brands PR

Since HA 2026.3 a custom integration serves its own brand images from a
`brand/` folder next to `manifest.json`, through HA's local brands proxy
(`/api/brands/integration/<domain>/icon.png`), and those take priority over
`brands.home-assistant.io` — see the
[brands proxy API announcement](https://developers.home-assistant.io/blog/2026/02/24/brands-proxy-api).
We require 2026.7.0 (`hacs.json`), so this is always available and a
[home-assistant/brands](https://github.com/home-assistant/brands) PR is
unnecessary.

`custom_components/pipewire_audio_router/brand/` therefore holds `icon.png`
256×256, `icon@2x.png` 512×512, `logo.png` 400×160, `logo@2x.png` 800×320 —
the sizes the brands repo mandates (icons exactly square; logos landscape with
the short side 128–256 normal / 256–512 hDPI). HACS copies the integration
folder verbatim, so they ship with a normal install.

Two things to know:

- The optional `dark_icon.png` / `dark_logo.png` variants are deliberately
  absent. The mark carries its own dark surface and was checked against both
  the light and the dark frontend, so one set covers both themes.
- HA's own pages (Settings → Devices & Services, device pages, config flows)
  render these. The HACS *downloads* panel still resolves icons via the old CDN
  URL and shows "icon not available" —
  [hacs/integration#5223](https://github.com/hacs/integration/issues/5223),
  open, a HACS-frontend bug that no change here can fix.
