#!/usr/bin/env bash
# Rasterize assets/app-icon.svg into AppIcon.icns and menubar-template.svg into
# the runtime PNG the menu-bar binary loads at startup.
#
# Outputs:
#   assets/AppIcon.icns                 (for the .app bundle)
#   assets/menubar-template.png         (16x16, loaded by the binary)
#   assets/menubar-template@2x.png      (32x32, for retina)
#
# Requires: rsvg-convert (or magick), iconutil.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

command -v rsvg-convert >/dev/null 2>&1 || { echo "need rsvg-convert: brew install librsvg" >&2; exit 1; }
command -v iconutil   >/dev/null 2>&1 || { echo "need iconutil (Xcode CLT)" >&2; exit 1; }

iconset="assets/AppIcon.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"

# Apple's required iconset names.
sizes=(
    "16 icon_16x16.png"
    "32 icon_16x16@2x.png"
    "32 icon_32x32.png"
    "64 icon_32x32@2x.png"
    "128 icon_128x128.png"
    "256 icon_128x128@2x.png"
    "256 icon_256x256.png"
    "512 icon_256x256@2x.png"
    "512 icon_512x512.png"
    "1024 icon_512x512@2x.png"
)
for entry in "${sizes[@]}"; do
    size="${entry%% *}"
    name="${entry#* }"
    rsvg-convert -w "$size" -h "$size" assets/app-icon.svg -o "$iconset/$name"
done

iconutil -c icns "$iconset" -o assets/AppIcon.icns
rm -rf "$iconset"
echo "built assets/AppIcon.icns"

rsvg-convert -w 16 -h 16 assets/menubar-template.svg -o assets/menubar-template.png
rsvg-convert -w 32 -h 32 assets/menubar-template.svg -o assets/menubar-template@2x.png
echo "built assets/menubar-template.png (16x16 + 2x)"
