#!/usr/bin/env bash
# Render every branding PNG from the master SVGs in docs/branding/.
#
# The PNGs are committed (Supervisor and the home-assistant/brands repo only
# consume PNG), so re-run this after editing icon.svg / logo.svg and commit the
# result. Requires inkscape (rasteriser, honours the SVG's own font stack) and
# ImageMagick (metadata strip).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$repo/docs/branding"
addon="$repo/pipewire_audio_router"
ytmusic="$repo/ytmusic_receiver"
brand="$repo/custom_components/pipewire_audio_router/brand"
web="$addon/frontend/public"

command -v inkscape >/dev/null || { echo "inkscape not found" >&2; exit 1; }
command -v magick >/dev/null || { echo "ImageMagick (magick) not found" >&2; exit 1; }

mkdir -p "$brand" "$web"

render() { # svg width height out
  inkscape -w "$2" -h "$3" -o "$4" "$1" >/dev/null 2>&1
  magick "$4" -strip "$4"
  printf '  %-72s %s\n' "${4#"$repo"/}" "$(magick identify -format '%wx%h' "$4")"
}

echo "add-on (Supervisor store + sidebar):"
render "$src/icon.svg" 256 256 "$addon/icon.png"
render "$src/logo.svg" 500 200 "$addon/logo.png"

# Icon only, no logo: the store falls back to the icon on the detail page, and a
# "YouTube Music" wordmark set in our own type would read as Google's branding on an
# add-on that is not theirs.
echo "YouTube Music receiver add-on:"
render "$src/ytmusic-icon.svg" 256 256 "$ytmusic/icon.png"

# custom_components/<domain>/brand/ — served by HA's own brands proxy since
# 2026.3 and preferred over the CDN, so no home-assistant/brands PR is needed.
# Sizes follow the brands repo rules: icons exactly 256/512 square, logos
# landscape with the short side 128-256 (normal) and 256-512 (hDPI).
echo "custom integration (HA brands proxy):"
render "$src/icon.svg" 256 256 "$brand/icon.png"
render "$src/icon.svg" 512 512 "$brand/icon@2x.png"
render "$src/logo.svg" 400 160 "$brand/logo.png"
render "$src/logo.svg" 800 320 "$brand/logo@2x.png"

# The README embeds the raster, not logo.svg: the wordmark is set in Inter, and
# a viewer's browser substituting its own fallback font would reflow the tuned
# letter-spacing. Rasterising here pins the typography.
echo "README:"
render "$src/logo.svg" 800 320 "$src/logo.png"

echo "add-on web UI:"
cp "$src/icon.svg" "$web/favicon.svg"
printf '  %-72s %s\n' "${web#"$repo"/}/favicon.svg" "vector"
render "$src/icon.svg" 180 180 "$web/apple-touch-icon.png"
